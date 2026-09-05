// Package topology derives a bounded, deterministic graph from a source tree.
package topology

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"go/parser"
	"go/token"
	"io"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"unicode/utf8"
)

const (
	maxDepth                   = 32
	maxFiles                   = 50_000
	maxNodes                   = 4_096
	maxEdges                   = 16_384
	maxBytes             int64 = 1 << 30
	maxAnalyzerFileBytes       = 4 << 20
)

var ErrBounds = errors.New("topology bounds exceeded")

type NodeKind string

const (
	NodeRepository NodeKind = "repository"
	NodeModule     NodeKind = "module"
	NodePackage    NodeKind = "package"
	NodeDirectory  NodeKind = "directory"
)

type EdgeKind string

const (
	EdgeContains EdgeKind = "contains"
	EdgeImports  EdgeKind = "imports"
)

type Snapshot struct {
	Digest         string `json:"digest"`
	SourceRevision string `json:"source_revision,omitempty"`
	Nodes          []Node `json:"nodes"`
	Edges          []Edge `json:"edges"`

	fingerprint string
}

type Node struct {
	ID           string   `json:"id"`
	ParentID     string   `json:"parent_id"`
	Kind         NodeKind `json:"kind"`
	RelativePath string   `json:"relative_path"`
	Label        string   `json:"label"`
	Language     string   `json:"language"`
	SizeBucket   string   `json:"size_bucket"`
}

type Edge struct {
	From   string   `json:"from"`
	To     string   `json:"to"`
	Kind   EdgeKind `json:"kind"`
	Weight uint32   `json:"weight"`
}

type limits struct {
	depth, files, nodes, edges int
	bytes                      int64
}

var defaultLimits = limits{depth: maxDepth, files: maxFiles, nodes: maxNodes, edges: maxEdges, bytes: maxBytes}

type fileFact struct {
	Path string `json:"path"`
	Size int64  `json:"size"`
}

type manifestFact struct {
	Path string `json:"path"`
	Body []byte `json:"body"`
}

type goPackage struct {
	names, imports map[string]uint32
}

type jsPackage struct {
	name string
	deps map[string]struct{}
}

type languageFile struct{ dir, language string }

type discovery struct {
	dirs      map[string]int64
	files     []fileFact
	manifests []manifestFact
	modules   map[string]string
	goPkgs    map[string]*goPackage
	jsPkgs    map[string]jsPackage
	languages []languageFile
}

type packageFact struct {
	label                          string
	goCode, javascript, typescript bool
}

type pathEdge struct{ from, to string }

type analysis struct {
	packages map[string]packageFact
	imports  map[pathEdge]uint32
}

// Build scans root without running project commands. A previous snapshot from
// this package is returned unchanged when its structural fingerprint matches.
func Build(ctx context.Context, root string, previous *Snapshot) (Snapshot, error) {
	return build(ctx, root, previous, defaultLimits)
}

// NodeForPath returns the deepest topology node containing relativePath.
func NodeForPath(snapshot Snapshot, relativePath string) (Node, bool) {
	relativePath = path.Clean(filepath.ToSlash(relativePath))
	if relativePath == ".." || path.IsAbs(relativePath) || strings.HasPrefix(relativePath, "../") {
		return Node{}, false
	}
	bestDepth, bestKind := -1, -1
	var best Node
	for _, node := range snapshot.Nodes {
		if !contains(node.RelativePath, relativePath) {
			continue
		}
		nodeDepth, kind := depth(node.RelativePath), nodeKindOrder(node.Kind)
		if nodeDepth > bestDepth || nodeDepth == bestDepth && kind > bestKind {
			best, bestDepth, bestKind = node, nodeDepth, kind
		}
	}
	return best, bestDepth >= 0
}

func build(ctx context.Context, root string, previous *Snapshot, bounds limits) (Snapshot, error) {
	found, err := discover(ctx, root, bounds)
	if err != nil {
		return Snapshot{}, err
	}
	analyzed, err := analyze(found, bounds.edges)
	if err != nil {
		return Snapshot{}, err
	}
	fingerprint := fingerprint(found, analyzed)
	revision := sourceRevision(root)
	nodes, edges, err := graph(found, analyzed, bounds.nodes, bounds.edges)
	if err != nil {
		return Snapshot{}, err
	}
	result := Snapshot{SourceRevision: revision, Nodes: nodes, Edges: edges, fingerprint: fingerprint}
	result.Digest = graphDigest(nodes, edges)
	if previous != nil && previous.fingerprint == fingerprint && previous.SourceRevision == revision && previous.Digest == result.Digest && graphDigest(previous.Nodes, previous.Edges) == previous.Digest {
		return *previous, nil
	}
	return result, nil
}

func graphDigest(nodes []Node, edges []Edge) string {
	return digest("dark-factory/topology/graph/v1\x00", struct {
		Nodes []Node `json:"nodes"`
		Edges []Edge `json:"edges"`
	}{nodes, edges})
}

func discover(ctx context.Context, root string, bounds limits) (*discovery, error) {
	if bounds.depth < 0 || bounds.files < 1 || bounds.nodes < 1 || bounds.edges < 0 || bounds.bytes < 0 {
		return nil, fmt.Errorf("%w: invalid limits", ErrBounds)
	}
	info, err := os.Lstat(root)
	if err != nil {
		return nil, fmt.Errorf("inspect topology root: %w", err)
	}
	if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
		return nil, errors.New("topology root is not a directory")
	}
	root, err = filepath.Abs(root)
	if err != nil {
		return nil, fmt.Errorf("resolve topology root: %w", err)
	}
	root, err = filepath.EvalSymlinks(root)
	if err != nil {
		return nil, fmt.Errorf("resolve topology root symlinks: %w", err)
	}
	rootFS, err := os.OpenRoot(root)
	if err != nil {
		return nil, fmt.Errorf("open topology root: %w", err)
	}
	defer rootFS.Close()
	result := &discovery{
		dirs: make(map[string]int64), modules: make(map[string]string),
		goPkgs: make(map[string]*goPackage), jsPkgs: make(map[string]jsPackage),
	}
	var total int64
	err = filepath.WalkDir(root, func(name string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		// The walk can visit tens of thousands of entries. It runs on a request
		// with a deadline, so it stops when that deadline does.
		if err := ctx.Err(); err != nil {
			return err
		}
		rel, err := filepath.Rel(root, name)
		if err != nil {
			return err
		}
		rel = filepath.ToSlash(rel)
		if entry.IsDir() {
			if rel != "." && ignored(entry.Name()) {
				return filepath.SkipDir
			}
			if depth(rel) > bounds.depth {
				return bound("depth", bounds.depth)
			}
			if _, exists := result.dirs[rel]; !exists && len(result.dirs) == bounds.nodes {
				return bound("node count", bounds.nodes)
			}
			result.dirs[rel] = 0
			return nil
		}
		if entry.Type()&os.ModeSymlink != 0 {
			return nil
		}
		fileInfo, err := entry.Info()
		if err != nil {
			return err
		}
		if !fileInfo.Mode().IsRegular() {
			return nil
		}
		if depth(rel) > bounds.depth {
			return bound("depth", bounds.depth)
		}
		if len(result.files) == bounds.files {
			return bound("file count", bounds.files)
		}
		if fileInfo.Size() > bounds.bytes-total {
			return bound("byte count", bounds.bytes)
		}
		total += fileInfo.Size()
		result.files = append(result.files, fileFact{rel, fileInfo.Size()})
		dir := path.Dir(rel)
		result.dirs[dir] += fileInfo.Size()
		switch strings.ToLower(path.Ext(rel)) {
		case ".js", ".jsx", ".mjs", ".cjs":
			result.languages = append(result.languages, languageFile{dir, "javascript"})
		case ".ts", ".tsx", ".mts", ".cts":
			result.languages = append(result.languages, languageFile{dir, "typescript"})
		}
		base := path.Base(rel)
		if base != "go.mod" && base != "package.json" && path.Ext(rel) != ".go" {
			return nil
		}
		body, err := readSmallRoot(rootFS, rel, maxAnalyzerFileBytes)
		if errors.Is(err, ErrBounds) {
			// A generated or vendored file past the analyzer bound contributes no
			// imports rather than failing the whole tree. Its bytes already count
			// toward its directory, and the tree-wide bounds stay hard stops. The
			// body must stay non-nil: a nil one sends the parser to the disk.
			body, err = []byte{}, nil
		}
		if err != nil {
			return err
		}
		if base == "go.mod" {
			result.manifests = append(result.manifests, manifestFact{rel, body})
			result.modules[dir] = moduleName(body)
			return nil
		}
		if base == "package.json" {
			result.manifests = append(result.manifests, manifestFact{rel, body})
			result.jsPkgs[dir] = parseJSPackage(body)
			return nil
		}
		pkg := result.goPkgs[dir]
		if pkg == nil {
			pkg = &goPackage{make(map[string]uint32), make(map[string]uint32)}
			result.goPkgs[dir] = pkg
		}
		parsed, _ := parser.ParseFile(token.NewFileSet(), rel, body, parser.ImportsOnly)
		if parsed != nil {
			// A file with no package clause the parser could read — an oversize
			// one included — reports the empty name, which would otherwise win
			// the sorted vote and cost the package its name.
			if parsed.Name.Name != "" {
				pkg.names[parsed.Name.Name]++
			}
			for _, imported := range parsed.Imports {
				if value, err := strconv.Unquote(imported.Path.Value); err == nil {
					pkg.imports[value]++
				}
			}
		}
		return nil
	})
	if err != nil {
		if errors.Is(err, ErrBounds) {
			return nil, err
		}
		return nil, fmt.Errorf("discover topology: %w", err)
	}
	dirs := keys(result.dirs)
	sort.Slice(dirs, func(i, j int) bool { return depth(dirs[i]) > depth(dirs[j]) })
	for _, dir := range dirs {
		if dir != "." {
			result.dirs[path.Dir(dir)] += result.dirs[dir]
		}
	}
	return result, nil
}

func analyze(found *discovery, edgeLimit int) (analysis, error) {
	result := analysis{make(map[string]packageFact), make(map[pathEdge]uint32)}
	addImport := func(edge pathEdge, weight uint32) error {
		if _, exists := result.imports[edge]; !exists && len(result.imports) == edgeLimit {
			return bound("edge count", edgeLimit)
		}
		result.imports[edge] += weight
		return nil
	}
	for dir, manifest := range found.jsPkgs {
		result.packages[dir] = packageFact{label: label(manifest.name, dir, "package"), javascript: true}
	}
	for dir, pkg := range found.goPkgs {
		if _, _, ok := nearestModule(dir, found.modules); !ok {
			continue
		}
		fact := result.packages[dir]
		if fact.label == "" {
			fact.label = label(first(keys(pkg.names)), dir, "package")
		}
		fact.goCode = true
		result.packages[dir] = fact
	}
	jsRoots := keys(found.jsPkgs)
	sort.Slice(jsRoots, func(i, j int) bool { return depth(jsRoots[i]) > depth(jsRoots[j]) })
	for _, file := range found.languages {
		for _, root := range jsRoots {
			if contains(root, file.dir) {
				fact := result.packages[root]
				fact.typescript = fact.typescript || file.language == "typescript"
				fact.javascript = fact.javascript || file.language == "javascript"
				result.packages[root] = fact
				break
			}
		}
	}
	localGo := make(map[string][]string)
	for dir, pkg := range result.packages {
		moduleDir, moduleName, ok := nearestModule(dir, found.modules)
		if !pkg.goCode || !ok || moduleName == "" {
			continue
		}
		name := moduleName
		if suffix := relative(moduleDir, dir); suffix != "." {
			name += "/" + suffix
		}
		localGo[name] = append(localGo[name], dir)
	}
	for from, pkg := range found.goPkgs {
		for imported, weight := range pkg.imports {
			if targets := localGo[imported]; result.packages[from].goCode && len(targets) == 1 && targets[0] != from {
				if err := addImport(pathEdge{from, targets[0]}, weight); err != nil {
					return analysis{}, err
				}
			}
		}
	}
	localJS := make(map[string][]string)
	for dir, pkg := range found.jsPkgs {
		if pkg.name != "" {
			localJS[pkg.name] = append(localJS[pkg.name], dir)
		}
	}
	for from, pkg := range found.jsPkgs {
		for dependency := range pkg.deps {
			if targets := localJS[dependency]; len(targets) == 1 && targets[0] != from {
				if err := addImport(pathEdge{from, targets[0]}, 1); err != nil {
					return analysis{}, err
				}
			}
		}
	}
	return result, nil
}

func graph(found *discovery, analyzed analysis, nodeLimit, edgeLimit int) ([]Node, []Edge, error) {
	add := func(nodes *[]Node, node Node) error {
		if len(*nodes) == nodeLimit {
			return bound("node count", nodeLimit)
		}
		*nodes = append(*nodes, node)
		return nil
	}
	repository := nodeID(NodeRepository, ".")
	nodes := []Node{{ID: repository, Kind: NodeRepository, RelativePath: ".", Label: "repository", SizeBucket: bucket(found.dirs["."])}}
	primary := map[string]string{".": repository}
	dirs := keys(found.dirs)
	sort.Slice(dirs, func(i, j int) bool {
		return depth(dirs[i]) < depth(dirs[j]) || depth(dirs[i]) == depth(dirs[j]) && dirs[i] < dirs[j]
	})
	for _, dir := range dirs {
		parent := repository
		if dir != "." {
			parent = primary[path.Dir(dir)]
		}
		moduleName, module := found.modules[dir]
		pkg, isPackage := analyzed.packages[dir]
		if module {
			id := nodeID(NodeModule, dir)
			if err := add(&nodes, Node{id, parent, NodeModule, dir, label(moduleName, dir, "module"), "go", bucket(found.dirs[dir])}); err != nil {
				return nil, nil, err
			}
			primary[dir], parent = id, id
		}
		if isPackage {
			id := nodeID(NodePackage, dir)
			if err := add(&nodes, Node{id, parent, NodePackage, dir, pkg.label, language(pkg), bucket(found.dirs[dir])}); err != nil {
				return nil, nil, err
			}
			if !module {
				primary[dir] = id
			}
		}
		if dir != "." && !module && !isPackage {
			id := nodeID(NodeDirectory, dir)
			if err := add(&nodes, Node{id, parent, NodeDirectory, dir, path.Base(dir), "", bucket(found.dirs[dir])}); err != nil {
				return nil, nil, err
			}
			primary[dir] = id
		}
	}
	sort.Slice(nodes, func(i, j int) bool {
		return nodes[i].RelativePath < nodes[j].RelativePath || nodes[i].RelativePath == nodes[j].RelativePath && nodes[i].Kind < nodes[j].Kind
	})
	containmentEdges := len(nodes) - 1
	if containmentEdges > edgeLimit || len(analyzed.imports) > edgeLimit-containmentEdges {
		return nil, nil, bound("edge count", edgeLimit)
	}
	edges := make([]Edge, 0, len(nodes)-1+len(analyzed.imports))
	for _, node := range nodes {
		if node.ParentID != "" {
			edges = append(edges, Edge{node.ParentID, node.ID, EdgeContains, 1})
		}
	}
	for edge, weight := range analyzed.imports {
		edges = append(edges, Edge{nodeID(NodePackage, edge.from), nodeID(NodePackage, edge.to), EdgeImports, weight})
	}
	sort.Slice(edges, func(i, j int) bool {
		return edges[i].From < edges[j].From || edges[i].From == edges[j].From && (edges[i].To < edges[j].To || edges[i].To == edges[j].To && edges[i].Kind < edges[j].Kind)
	})
	return nodes, edges, nil
}

func fingerprint(found *discovery, analyzed analysis) string {
	type packageInput struct{ Path, Label, Language string }
	type importInput struct {
		From, To string
		Weight   uint32
	}
	input := struct {
		Directories []string
		Files       []fileFact
		Manifests   []manifestFact
		Packages    []packageInput
		Imports     []importInput
	}{Directories: keys(found.dirs), Files: found.files, Manifests: found.manifests}
	sort.Slice(input.Files, func(i, j int) bool { return input.Files[i].Path < input.Files[j].Path })
	sort.Slice(input.Manifests, func(i, j int) bool { return input.Manifests[i].Path < input.Manifests[j].Path })
	for dir, pkg := range analyzed.packages {
		input.Packages = append(input.Packages, packageInput{dir, pkg.label, language(pkg)})
	}
	for edge, weight := range analyzed.imports {
		input.Imports = append(input.Imports, importInput{edge.from, edge.to, weight})
	}
	sort.Slice(input.Packages, func(i, j int) bool { return input.Packages[i].Path < input.Packages[j].Path })
	sort.Slice(input.Imports, func(i, j int) bool {
		return input.Imports[i].From < input.Imports[j].From || input.Imports[i].From == input.Imports[j].From && input.Imports[i].To < input.Imports[j].To
	})
	return digest("dark-factory/topology/fingerprint/v1\x00", input)
}

func parseJSPackage(body []byte) jsPackage {
	var value struct {
		Name                 string            `json:"name"`
		Dependencies         map[string]string `json:"dependencies"`
		DevDependencies      map[string]string `json:"devDependencies"`
		PeerDependencies     map[string]string `json:"peerDependencies"`
		OptionalDependencies map[string]string `json:"optionalDependencies"`
	}
	result := jsPackage{deps: make(map[string]struct{})}
	if json.Unmarshal(body, &value) != nil {
		return result
	}
	result.name = strings.TrimSpace(value.Name)
	for _, dependencies := range []map[string]string{value.Dependencies, value.DevDependencies, value.PeerDependencies, value.OptionalDependencies} {
		for name := range dependencies {
			result.deps[name] = struct{}{}
		}
	}
	return result
}

func moduleName(body []byte) string {
	for _, line := range strings.Split(string(body), "\n") {
		fields := strings.Fields(line)
		if len(fields) >= 2 && fields[0] == "module" {
			if value, err := strconv.Unquote(fields[1]); err == nil {
				return value
			}
			return fields[1]
		}
	}
	return ""
}

func nearestModule(dir string, modules map[string]string) (string, string, bool) {
	for current := dir; ; current = path.Dir(current) {
		if name, ok := modules[current]; ok {
			return current, name, true
		}
		if current == "." {
			return "", "", false
		}
	}
}

func sourceRevision(root string) string {
	gitDir := filepath.Join(root, ".git")
	info, err := os.Lstat(gitDir)
	if err != nil || info.Mode()&os.ModeSymlink != 0 {
		return ""
	}
	if !info.IsDir() {
		body, err := readSmall(gitDir, 4096)
		value := strings.TrimSpace(string(body))
		if err != nil || !strings.HasPrefix(value, "gitdir: ") {
			return ""
		}
		gitDir = strings.TrimSpace(strings.TrimPrefix(value, "gitdir: "))
		if !filepath.IsAbs(gitDir) {
			gitDir = filepath.Join(root, gitDir)
		}
	}
	head, err := readSmall(filepath.Join(gitDir, "HEAD"), 4096)
	if err != nil {
		return ""
	}
	value := strings.TrimSpace(string(head))
	if oid := gitOID(value); oid != "" {
		return oid
	}
	ref := strings.TrimSpace(strings.TrimPrefix(value, "ref: "))
	if !strings.HasPrefix(value, "ref: refs/") || path.Clean(ref) != ref || strings.Contains(ref, "..") {
		return ""
	}
	search := []string{gitDir}
	if body, err := readSmall(filepath.Join(gitDir, "commondir"), 4096); err == nil {
		common := strings.TrimSpace(string(body))
		if !filepath.IsAbs(common) {
			common = filepath.Join(gitDir, common)
		}
		search = append(search, common)
	}
	for _, directory := range search {
		if body, err := readSmall(filepath.Join(directory, filepath.FromSlash(ref)), 4096); err == nil {
			if oid := gitOID(strings.TrimSpace(string(body))); oid != "" {
				return oid
			}
		}
		if body, err := readSmall(filepath.Join(directory, "packed-refs"), 4<<20); err == nil {
			for _, line := range strings.Split(string(body), "\n") {
				fields := strings.Fields(line)
				if len(fields) == 2 && fields[1] == ref {
					return gitOID(fields[0])
				}
			}
		}
	}
	return ""
}

func readSmall(name string, limit int64) ([]byte, error) {
	before, err := os.Lstat(name)
	if err != nil {
		return nil, err
	}
	if !before.Mode().IsRegular() {
		return nil, errNotRegularFile
	}
	file, err := os.Open(name)
	if err != nil {
		return nil, err
	}
	return readOpened(file, before, limit)
}

var errNotRegularFile = errors.New("not a regular file")

func readSmallRoot(root *os.Root, name string, limit int64) ([]byte, error) {
	name = filepath.FromSlash(name)
	before, err := root.Lstat(name)
	if err != nil {
		return nil, err
	}
	if !before.Mode().IsRegular() {
		return nil, errNotRegularFile
	}
	file, err := root.Open(name)
	if err != nil {
		return nil, err
	}
	return readOpened(file, before, limit)
}

func readOpened(file *os.File, before fs.FileInfo, limit int64) ([]byte, error) {
	defer file.Close()
	after, err := file.Stat()
	if err != nil {
		return nil, err
	}
	if !after.Mode().IsRegular() || !os.SameFile(before, after) {
		return nil, errNotRegularFile
	}
	body, err := io.ReadAll(io.LimitReader(file, limit+1))
	if err == nil && int64(len(body)) > limit {
		err = ErrBounds
	}
	return body, err
}

// A dot directory at any depth is tooling state, not the project's code: .git,
// caches, and local toolchains such as .tools all hide there. The root itself
// is exempt, so a project checked out under a dot directory still builds.
func ignored(name string) bool {
	if strings.HasPrefix(name, ".") {
		return true
	}
	switch name {
	case "node_modules", "vendor", "dist", "build", "target", "coverage", "__pycache__":
		return true
	}
	return false
}

func label(value, rel, fallback string) string {
	value = strings.TrimSpace(value)
	if value != "" && len(value) <= 256 && utf8.ValidString(value) && !path.IsAbs(value) && !filepath.IsAbs(value) && filepath.VolumeName(value) == "" && !strings.ContainsAny(value, "\x00\r\n") {
		return value
	}
	if rel == "." {
		return fallback
	}
	return path.Base(rel)
}

func language(pkg packageFact) string {
	if pkg.goCode && (pkg.javascript || pkg.typescript) {
		return "mixed"
	}
	if pkg.goCode {
		return "go"
	}
	if pkg.typescript {
		return "typescript"
	}
	return "javascript"
}

func bucket(bytes int64) string {
	switch {
	case bytes == 0:
		return "empty"
	case bytes <= 4<<10:
		return "tiny"
	case bytes <= 64<<10:
		return "small"
	case bytes <= 1<<20:
		return "medium"
	default:
		return "large"
	}
}

func nodeID(kind NodeKind, rel string) string {
	sum := sha256.Sum256([]byte("dark-factory/topology/node/v1\x00" + string(kind) + "\x00" + rel))
	return hex.EncodeToString(sum[:])
}

func nodeKindOrder(kind NodeKind) int {
	switch kind {
	case NodePackage:
		return 3
	case NodeModule:
		return 2
	case NodeDirectory:
		return 1
	case NodeRepository:
		return 0
	default:
		return -1
	}
}

func digest(domain string, value any) string {
	body, _ := json.Marshal(value)
	sum := sha256.Sum256(append([]byte(domain), body...))
	return hex.EncodeToString(sum[:])
}

func gitOID(value string) string {
	decoded, err := hex.DecodeString(value)
	if err != nil || len(decoded) != 20 && len(decoded) != 32 {
		return ""
	}
	return strings.ToLower(value)
}

func depth(rel string) int {
	if rel == "." {
		return 0
	}
	return strings.Count(rel, "/") + 1
}

func contains(parent, child string) bool {
	return parent == "." || parent == child || strings.HasPrefix(child, parent+"/")
}

func relative(parent, child string) string {
	if parent == "." {
		return child
	}
	if parent == child {
		return "."
	}
	return strings.TrimPrefix(child, parent+"/")
}

func bound(name string, limit any) error {
	return fmt.Errorf("%w: %s exceeds %v", ErrBounds, name, limit)
}

func first[T any](values []T) (zero T) {
	if len(values) != 0 {
		return values[0]
	}
	return zero
}

func keys[V any](values map[string]V) []string {
	result := make([]string, 0, len(values))
	for key := range values {
		result = append(result, key)
	}
	sort.Strings(result)
	return result
}
