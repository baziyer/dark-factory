package browser

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

const (
	Path           = "/browser"
	PairPath       = "/pair"
	maxOrigins     = 8
	maxConnections = 32
	// One coherent snapshot is one request, and a notification burst
	// collapses into at most one trailing refresh. 1,024 retains every
	// request ID for the whole connection while leaving ample room for
	// refreshes, detail reads, terminal control and task enqueue.
	maxRequests         = 1024
	readQueueSize       = 8
	maxHeaderBytes      = 8 << 10
	authenticationLimit = 5 * time.Second
	writeLimit          = 2 * time.Second
	readHeaderLimit     = 2 * time.Second
	shutdownHeaderLimit = 2 * time.Second
	implementedCaps     = browserprotocol.CapabilityObserve | browserprotocol.CapabilityPrivateHumanRequestDetail | browserprotocol.CapabilityHumanActions | browserprotocol.CapabilityTerminalInput
)

type Config struct {
	Address        string
	AllowedOrigins []string
	Backend        Backend
}

type clientLifecycle struct {
	connections    map[*connection]struct{}
	authenticating map[*connection]struct{}
	revoking       int
}

type Server struct {
	backend            Backend
	terminalBackend    TerminalBackend
	taskBackend        TaskBackend
	consoleBackend     ConsoleBackend
	pairBackend        PairBackend
	pairPolicy         string
	host               string
	origins            map[string]struct{}
	terminalAckTimeout time.Duration
	http               *http.Server

	ctx    context.Context
	cancel context.CancelFunc

	mu              sync.Mutex
	closing         bool
	connections     map[*connection]struct{}
	clientLifecycle map[[browserprotocol.ClientIDSize]byte]*clientLifecycle
	pairing         map[*connection]struct{}
	pairingBlocked  int
	slots           chan struct{}
	serveDone       chan struct{}
	serveErr        error
	cleanupErr      error
	closeErr        error
	closeOnce       sync.Once
}

func Listen(config Config) (*Server, error) {
	if config.Backend == nil {
		return nil, fmt.Errorf("browser: backend is required")
	}
	if err := validateAddress(config.Address); err != nil {
		return nil, err
	}
	origins, err := validateOrigins(config.AllowedOrigins)
	if err != nil {
		return nil, err
	}
	listener, err := net.Listen("tcp4", config.Address)
	if err != nil {
		return nil, fmt.Errorf("browser: listen: %w", err)
	}
	return start(config.Backend, origins, listener), nil
}

func start(backend Backend, origins map[string]struct{}, listener net.Listener) *Server {
	ctx, cancel := context.WithCancel(context.Background())
	server := &Server{
		backend:            backend,
		terminalBackend:    func() TerminalBackend { value, _ := backend.(TerminalBackend); return value }(),
		taskBackend:        func() TaskBackend { value, _ := backend.(TaskBackend); return value }(),
		consoleBackend:     func() ConsoleBackend { value, _ := backend.(ConsoleBackend); return value }(),
		pairBackend:        func() PairBackend { value, _ := backend.(PairBackend); return value }(),
		pairPolicy:         pairPolicy(origins),
		host:               listener.Addr().String(),
		origins:            origins,
		terminalAckTimeout: time.Duration(browserprotocol.TerminalAckTimeoutMS) * time.Millisecond,
		ctx:                ctx,
		cancel:             cancel,
		connections:        make(map[*connection]struct{}),
		clientLifecycle:    make(map[[browserprotocol.ClientIDSize]byte]*clientLifecycle),
		pairing:            make(map[*connection]struct{}),
		slots:              make(chan struct{}, maxConnections),
		serveDone:          make(chan struct{}),
	}
	server.http = &http.Server{
		Handler:           http.HandlerFunc(server.handle),
		ReadHeaderTimeout: readHeaderLimit,
		IdleTimeout:       shutdownHeaderLimit,
		MaxHeaderBytes:    maxHeaderBytes,
	}
	go func() {
		defer close(server.serveDone)
		if err := server.http.Serve(listener); err != nil && !errors.Is(err, http.ErrServerClosed) {
			server.mu.Lock()
			server.serveErr = fmt.Errorf("browser: serve: %w", err)
			server.closing = true
			connections := make([]*connection, 0, len(server.connections))
			for current := range server.connections {
				connections = append(connections, current)
			}
			server.mu.Unlock()
			server.cancel()
			for _, current := range connections {
				current.stop()
			}
		}
	}()
	return server
}

func (server *Server) Addr() string {
	if server == nil {
		return ""
	}
	return server.host
}

// ServeDone closes if the listener stops, including an unexpected Serve
// failure. Err reports that bounded failure without exposing request data.
func (server *Server) ServeDone() <-chan struct{} {
	if server == nil {
		closed := make(chan struct{})
		close(closed)
		return closed
	}
	return server.serveDone
}

func (server *Server) Err() error {
	if server == nil {
		return nil
	}
	server.mu.Lock()
	defer server.mu.Unlock()
	return server.serveErr
}

func (server *Server) Close() error {
	if server == nil {
		return nil
	}
	server.closeOnce.Do(func() {
		var closeErrors []error
		server.mu.Lock()
		server.closing = true
		connections := make([]*connection, 0, len(server.connections))
		for current := range server.connections {
			connections = append(connections, current)
		}
		server.mu.Unlock()
		server.cancel()
		if err := server.http.Close(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			closeErrors = append(closeErrors, err)
		}
		for _, current := range connections {
			current.stop()
		}
		for _, current := range connections {
			<-current.done
		}
		<-server.serveDone
		server.mu.Lock()
		if server.serveErr != nil {
			closeErrors = append(closeErrors, server.serveErr)
		}
		if server.cleanupErr != nil {
			closeErrors = append(closeErrors, server.cleanupErr)
		}
		server.closeErr = errors.Join(closeErrors...)
		server.mu.Unlock()
	})
	server.mu.Lock()
	defer server.mu.Unlock()
	return server.closeErr
}

// CloseClient is the non-reentrant revocation seam. The daemon commits
// revocation before calling it; this method performs no authorization call.
func (server *Server) CloseClient(clientID [browserprotocol.ClientIDSize]byte) error {
	if server == nil || zero16(clientID) {
		return nil
	}
	server.mu.Lock()
	lifecycle := server.lifecycleLocked(clientID)
	lifecycle.revoking++
	server.pairingBlocked++
	set := make(map[*connection]struct{})
	for current := range lifecycle.connections {
		set[current] = struct{}{}
	}
	for current := range lifecycle.authenticating {
		set[current] = struct{}{}
	}
	// A pairing call has no client ID until Backend returns its daemon-minted
	// result. Fence and join all calls already in flight so none can register
	// this client after revocation returns.
	for current := range server.pairing {
		set[current] = struct{}{}
	}
	connections := make([]*connection, 0, len(set))
	for current := range set {
		connections = append(connections, current)
	}
	server.mu.Unlock()
	for _, current := range connections {
		current.stop()
	}
	for _, current := range connections {
		<-current.done
	}
	var closeErrors []error
	for _, current := range connections {
		if current.cleanupErr != nil {
			closeErrors = append(closeErrors, current.cleanupErr)
		}
	}
	server.mu.Lock()
	lifecycle = server.clientLifecycle[clientID]
	if lifecycle != nil {
		lifecycle.revoking--
		server.removeLifecycleIfIdleLocked(clientID)
	}
	server.pairingBlocked--
	server.mu.Unlock()
	return errors.Join(closeErrors...)
}

func (server *Server) handle(writer http.ResponseWriter, request *http.Request) {
	if request.URL.Path == PairPath {
		server.handlePair(writer, request)
		return
	}
	if request.URL.Path != Path || request.URL.EscapedPath() != Path || request.URL.RawQuery != "" {
		http.Error(writer, http.StatusText(http.StatusNotFound), http.StatusNotFound)
		return
	}
	origin, ok := server.validRequest(request)
	if !ok {
		http.Error(writer, http.StatusText(http.StatusForbidden), http.StatusForbidden)
		return
	}
	select {
	case server.slots <- struct{}{}:
	default:
		http.Error(writer, http.StatusText(http.StatusServiceUnavailable), http.StatusServiceUnavailable)
		return
	}
	reserved := true
	defer func() {
		if reserved {
			<-server.slots
		}
	}()
	server.mu.Lock()
	closing := server.closing
	server.mu.Unlock()
	if closing {
		http.Error(writer, http.StatusText(http.StatusServiceUnavailable), http.StatusServiceUnavailable)
		return
	}
	websocketConnection, err := websocket.Accept(writer, request, &websocket.AcceptOptions{
		OriginPatterns:  []string{origin},
		CompressionMode: websocket.CompressionDisabled,
	})
	if err != nil {
		return
	}
	websocketConnection.SetReadLimit(browserprotocol.MaxControlBytes)
	ctx, cancel := context.WithCancel(server.ctx)
	current := &connection{
		server: server,
		ws:     websocketConnection,
		host:   server.host,
		origin: origin,
		ctx:    ctx,
		cancel: cancel,
		frames: make(chan incoming, readQueueSize),
		done:   make(chan struct{}),
	}
	server.mu.Lock()
	if server.closing {
		server.mu.Unlock()
		cancel()
		_ = websocketConnection.CloseNow()
		return
	}
	server.connections[current] = struct{}{}
	server.mu.Unlock()
	reserved = false
	defer func() { <-server.slots }()
	current.run()
}

// pairPage is the only HTML the daemon serves: one confirm page whose form
// mints a pairing link. It carries no script and no state; its stylesheet is
// pinned by hash so the policy below can forbid every other inline source.
const pairStyle = `body{margin:0;min-height:100vh;display:grid;place-items:center;background:#0b0d10;color:#d7dde3;font:16px/1.5 ui-monospace,SFMono-Regular,Menlo,monospace}main{max-width:32rem;padding:2rem}h1{font-size:1.25rem;letter-spacing:.08em;margin:0 0 1rem}p{margin:0 0 1.5rem}button{font:inherit;letter-spacing:.08em;padding:.75rem 1.5rem;background:#d7dde3;color:#0b0d10;border:0;cursor:pointer}`

const pairPage = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Pair this browser</title><style>" + pairStyle + "</style><main><h1>PAIR THIS BROWSER</h1><p>Dark Factory on this machine will trust this browser to operate the factory at app.darkfactory.build. Continue only if you opened this page yourself.</p><form method=post action=" + PairPath + "><button>PAIR THIS BROWSER</button></form></main>"

// pairPolicy admits the page's own form post and, because CSP checks
// form-action against every response in the submission's redirect chain, the
// console origins the mint redirects to; nothing else may load or run.
func pairPolicy(origins map[string]struct{}) string {
	targets := make([]string, 0, len(origins))
	for origin := range origins {
		targets = append(targets, origin)
	}
	sort.Strings(targets)
	digest := sha256.Sum256([]byte(pairStyle))
	return "default-src 'none'; style-src 'sha256-" + base64.StdEncoding.EncodeToString(digest[:]) + "'; form-action 'self' " + strings.Join(targets, " ") + "; base-uri 'none'; frame-ancestors 'none'"
}

// handlePair admits exactly two requests, both proven by the browser's own
// Fetch Metadata rather than by anything the page could carry: a top-level
// document navigation to the page from the address bar (Sec-Fetch-Site none)
// or from an allowed console origin (its Referer), and the page's own
// same-origin form post, which mints the link and redirects to the console.
// Everything else is 404 and mints nothing: fetch and XHR (mode cors), frames
// (dest iframe), cross-site form posts, and navigations that arrived from any
// other origin. Sec-Fetch-User is deliberately not required: Safari 26.5
// sent none of it on an OS-launched navigation (Sec-Fetch-Site none, Mode
// navigate, Dest document) nor on a real button-click POST (Site
// same-origin), while Chrome 151 sent "?1" on both; the Referer rule already
// refuses a scripted navigation from elsewhere, and a scripted same-origin
// post needs script on a page that has none.
//
// These headers prove which browser context sent a request; they do not
// authenticate the sender as a browser. Any local process able to connect to
// this loopback port can send them and obtain a challenge, so the port is a
// same-machine trust boundary, as SECURITY.md records.
func (server *Server) handlePair(writer http.ResponseWriter, request *http.Request) {
	header := request.Header
	if server.pairBackend == nil || request.URL.EscapedPath() != PairPath || request.URL.RawQuery != "" || request.Host != server.host ||
		header.Get("Sec-Fetch-Mode") != "navigate" || header.Get("Sec-Fetch-Dest") != "document" {
		http.NotFound(writer, request)
		return
	}
	writer.Header().Set("Cache-Control", "no-store")
	writer.Header().Set("Referrer-Policy", "no-referrer")
	switch {
	case request.Method == http.MethodGet && (header.Get("Sec-Fetch-Site") == "none" || server.refererAllowed(header.Get("Referer"))):
		writer.Header().Set("Content-Type", "text/html; charset=utf-8")
		writer.Header().Set("Content-Security-Policy", server.pairPolicy)
		writer.Header().Set("X-Content-Type-Options", "nosniff")
		writer.Header().Set("X-Frame-Options", "DENY")
		_, _ = io.WriteString(writer, pairPage)
	case request.Method == http.MethodPost && header.Get("Sec-Fetch-Site") == "same-origin":
		link, err := server.pairBackend.PairLink(request.Context())
		if err != nil || !server.validPairLink(link) {
			http.Error(writer, http.StatusText(http.StatusServiceUnavailable), http.StatusServiceUnavailable)
			return
		}
		http.Redirect(writer, request, link, http.StatusSeeOther)
	default:
		http.NotFound(writer, request)
	}
}

// validPairLink proves the redirect target before it is sent: one allowed
// origin, the root path, and a fragment carrying exactly one challenge in
// lowercase hex. A mint that returns anything else fails the request rather
// than sending the browser somewhere this server did not choose.
func (server *Server) validPairLink(link string) bool {
	origin, challenge, found := strings.Cut(link, "/#df_pair=")
	if !found || len(challenge) != 2*browserprotocol.ChallengeSize || challenge != strings.ToLower(challenge) {
		return false
	}
	if _, err := hex.DecodeString(challenge); err != nil {
		return false
	}
	_, allowed := server.origins[origin]
	return allowed
}

func (server *Server) refererAllowed(referer string) bool {
	parsed, err := url.Parse(referer)
	if err != nil || parsed.Scheme == "" || parsed.Host == "" {
		return false
	}
	_, ok := server.origins[parsed.Scheme+"://"+parsed.Host]
	return ok
}

func (server *Server) validRequest(request *http.Request) (string, bool) {
	if request.Host != server.host {
		return "", false
	}
	values := request.Header.Values("Origin")
	if len(values) != 1 || values[0] == "null" {
		return "", false
	}
	_, ok := server.origins[values[0]]
	return values[0], ok
}

func (server *Server) beginAuthentication(current *connection, clientID [browserprotocol.ClientIDSize]byte) bool {
	server.mu.Lock()
	defer server.mu.Unlock()
	if server.closing || zero16(clientID) {
		return false
	}
	lifecycle := server.lifecycleLocked(clientID)
	if lifecycle.revoking != 0 {
		server.removeLifecycleIfIdleLocked(clientID)
		return false
	}
	lifecycle.authenticating[current] = struct{}{}
	current.authenticating = true
	current.authenticatingID = clientID
	return true
}

func (server *Server) beginPairing(current *connection) bool {
	server.mu.Lock()
	defer server.mu.Unlock()
	if server.closing || server.pairingBlocked != 0 {
		return false
	}
	server.pairing[current] = struct{}{}
	current.pairing = true
	return true
}

func (server *Server) finishAuthentication(current *connection, requested [browserprotocol.ClientIDSize]byte, result Authentication, accept bool) bool {
	server.mu.Lock()
	defer server.mu.Unlock()
	server.removeAuthenticatingLocked(current)
	if !accept || server.closing || result.Principal.ClientID != requested || result.Principal.ConnectionID.zero() {
		return false
	}
	lifecycle := server.lifecycleLocked(requested)
	if lifecycle.revoking != 0 {
		server.removeLifecycleIfIdleLocked(requested)
		return false
	}
	server.registerLocked(current, result.Principal)
	return true
}

func (server *Server) registerPair(current *connection, result Authentication, accept bool) bool {
	server.mu.Lock()
	defer server.mu.Unlock()
	server.removePairingLocked(current)
	if !accept || server.closing || zero16(result.Principal.ClientID) || result.Principal.ConnectionID.zero() {
		return false
	}
	lifecycle := server.lifecycleLocked(result.Principal.ClientID)
	if lifecycle.revoking != 0 {
		server.removeLifecycleIfIdleLocked(result.Principal.ClientID)
		return false
	}
	server.registerLocked(current, result.Principal)
	return true
}

func (server *Server) registerLocked(current *connection, principal Principal) {
	current.principal = principal
	current.authenticated = true
	server.lifecycleLocked(principal.ClientID).connections[current] = struct{}{}
}

func (server *Server) removeAuthenticatingLocked(current *connection) {
	if !current.authenticating {
		return
	}
	clientID := current.authenticatingID
	if lifecycle := server.clientLifecycle[clientID]; lifecycle != nil {
		delete(lifecycle.authenticating, current)
	}
	current.authenticating = false
	current.authenticatingID = [browserprotocol.ClientIDSize]byte{}
	server.removeLifecycleIfIdleLocked(clientID)
}

func (server *Server) removePairingLocked(current *connection) {
	if !current.pairing {
		return
	}
	delete(server.pairing, current)
	current.pairing = false
}

func (server *Server) lifecycleLocked(clientID [browserprotocol.ClientIDSize]byte) *clientLifecycle {
	lifecycle := server.clientLifecycle[clientID]
	if lifecycle == nil {
		lifecycle = &clientLifecycle{
			connections:    make(map[*connection]struct{}),
			authenticating: make(map[*connection]struct{}),
		}
		server.clientLifecycle[clientID] = lifecycle
	}
	return lifecycle
}

func (server *Server) removeLifecycleIfIdleLocked(clientID [browserprotocol.ClientIDSize]byte) {
	lifecycle := server.clientLifecycle[clientID]
	if lifecycle != nil && lifecycle.revoking == 0 && len(lifecycle.connections) == 0 && len(lifecycle.authenticating) == 0 {
		delete(server.clientLifecycle, clientID)
	}
}

func (server *Server) recordCleanup(err error) {
	server.mu.Lock()
	if server.cleanupErr == nil {
		// Retain one bounded typed failure, not an attacker-growable log.
		server.cleanupErr = err
	}
	server.mu.Unlock()
}

func (server *Server) unregister(current *connection) {
	server.mu.Lock()
	delete(server.connections, current)
	server.removePairingLocked(current)
	server.removeAuthenticatingLocked(current)
	if current.authenticated {
		clientID := current.principal.ClientID
		if lifecycle := server.clientLifecycle[clientID]; lifecycle != nil {
			delete(lifecycle.connections, current)
		}
		server.removeLifecycleIfIdleLocked(clientID)
		current.authenticated = false
		current.principal = Principal{}
	}
	server.mu.Unlock()
}

func validateAddress(address string) error {
	host, port, err := net.SplitHostPort(address)
	if err != nil || host != "127.0.0.1" || port == "" {
		return fmt.Errorf("browser: address must be exact IPv4 loopback with a port")
	}
	parsed, err := strconv.Atoi(port)
	if err != nil || parsed < 0 || parsed > 65535 || strconv.Itoa(parsed) != port {
		return fmt.Errorf("browser: invalid loopback port")
	}
	return nil
}

func validateOrigins(values []string) (map[string]struct{}, error) {
	if len(values) == 0 || len(values) > maxOrigins {
		return nil, fmt.Errorf("browser: require one to %d exact origins", maxOrigins)
	}
	result := make(map[string]struct{}, len(values))
	for _, value := range values {
		parsed, err := url.Parse(value)
		if err != nil || parsed.Scheme != "https" && parsed.Scheme != "http" || parsed.Host == "" || parsed.User != nil || parsed.Path != "" || parsed.RawQuery != "" || parsed.Fragment != "" || parsed.String() != value || strings.Contains(value, "*") || len(value) > browserprotocol.MaxTextBytes {
			return nil, fmt.Errorf("browser: invalid exact origin")
		}
		if _, duplicate := result[value]; duplicate {
			return nil, fmt.Errorf("browser: duplicate origin")
		}
		result[value] = struct{}{}
	}
	return result, nil
}

func randomNonce() ([browserprotocol.NonceSize]byte, error) {
	var value [browserprotocol.NonceSize]byte
	_, err := rand.Read(value[:])
	return value, err
}

func newConnectionID() (ConnectionID, error) {
	var id ConnectionID
	if _, err := rand.Read(id.value[:]); err != nil {
		return ConnectionID{}, err
	}
	if id.zero() {
		return ConnectionID{}, fmt.Errorf("browser: random connection identity is zero")
	}
	return id, nil
}

func nonzero(value []byte) bool {
	for _, item := range value {
		if item != 0 {
			return true
		}
	}
	return false
}

func validateAuthentication(result Authentication) error {
	// A backend proves only durable client authority. Accepting or overwriting a
	// backend-selected connection identity would conceal an authority-boundary
	// violation, so fail closed before transport registration instead.
	if zero16(result.Principal.ClientID) || !result.Principal.ConnectionID.zero() || result.Capabilities&browserprotocol.CapabilityObserve == 0 || result.Capabilities&^implementedCaps != 0 {
		return ErrUnauthorized
	}
	return nil
}

func errorFrame(err error) browserprotocol.Error {
	switch {
	case errors.Is(err, ErrUnauthorized):
		return browserprotocol.Error{Code: browserprotocol.ErrorUnauthorized}
	case errors.Is(err, ErrNotFound):
		return browserprotocol.Error{Code: browserprotocol.ErrorNotFound}
	case errors.Is(err, ErrStale):
		return browserprotocol.Error{Code: browserprotocol.ErrorStale}
	case errors.Is(err, ErrInvalidRequest):
		return browserprotocol.Error{Code: browserprotocol.ErrorInvalidRequest}
	case errors.Is(err, ErrTooLarge):
		return browserprotocol.Error{Code: browserprotocol.ErrorTooLarge}
	case errors.Is(err, ErrRateLimited):
		return browserprotocol.Error{Code: browserprotocol.ErrorRateLimited, Retryable: true}
	default:
		return browserprotocol.Error{Code: browserprotocol.ErrorInternal}
	}
}

func zero16(value [16]byte) bool {
	for _, item := range value {
		if item != 0 {
			return false
		}
	}
	return true
}
