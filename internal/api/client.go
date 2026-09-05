package api

import (
	"bytes"
	"context"
	"crypto/subtle"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"net"
	"os"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/install"
)

// A frame's prelude is its auth domain and nothing else. There is no protocol
// generation: factoryctl, factory-runner and factoryd are siblings of one
// build, and the sibling-binary boundary already refuses a mixed installation.
const (
	operatorDomain      byte = 1
	attemptDomain       byte = 2
	requestPrelude           = 1 + credentialBytes
	responsePrelude          = 1
	outcomeReceiptBytes      = 32
	requestTimeout           = 5 * time.Second
	attemptTokenFileEnv      = "DARK_FACTORY_ATTEMPT_TOKEN_FILE"
)

type credential [credentialBytes]byte

func (credential) String() string   { return "credential(<redacted>)" }
func (credential) GoString() string { return "credential(<redacted>)" }

type client struct {
	socketPath string
	tokenPath  string
	token      tokenRecord
	socket     socketRecord
	domain     byte
}

type OperatorClient struct{ client client }
type AttemptClient struct{ client client }

func (OperatorClient) String() string   { return "OperatorClient(<redacted>)" }
func (OperatorClient) GoString() string { return "OperatorClient(<redacted>)" }
func (AttemptClient) String() string    { return "AttemptClient(<redacted>)" }
func (AttemptClient) GoString() string  { return "AttemptClient(<redacted>)" }

func NewOperatorClient(socketPath, tokenPath string) (*OperatorClient, error) {
	base, err := newClient(socketPath, tokenPath, operatorDomain)
	if err != nil {
		return nil, err
	}
	return &OperatorClient{client: base}, nil
}

// NewAttemptClientFromEnvironment reads only DARK_FACTORY_ATTEMPT_TOKEN_FILE.
// It has no operator-token locator and therefore cannot fall back across auth
// domains when the attempt credential is absent or invalid.
func NewAttemptClientFromEnvironment(socketPath string) (*AttemptClient, error) {
	tokenPath, found := os.LookupEnv(attemptTokenFileEnv)
	if !found || tokenPath == "" {
		return nil, ErrInvalidClient
	}
	base, err := newClient(socketPath, tokenPath, attemptDomain)
	if err != nil {
		return nil, err
	}
	return &AttemptClient{client: base}, nil
}

func newClient(socketPath, tokenPath string, domain byte) (client, error) {
	if domain != operatorDomain && domain != attemptDomain || !validCanonicalPath(socketPath, install.MaxSocketPathBytes) {
		return client{}, ErrInvalidClient
	}
	token, err := loadToken(tokenPath)
	if err != nil {
		return client{}, err
	}
	socket, err := inspectSocket(socketPath)
	if err != nil {
		return client{}, err
	}
	return client{socketPath: socketPath, tokenPath: tokenPath, token: token, socket: socket, domain: domain}, nil
}

func (client *OperatorClient) Health(ctx context.Context) (HealthStatus, error) {
	var result HealthStatus
	err := client.client.call(ctx, "health", struct{}{}, &result)
	return result, err
}

func (client *OperatorClient) WebStatus(ctx context.Context) (WebStatus, error) {
	var result WebStatus
	if err := client.client.call(ctx, "web_status", struct{}{}, &result); err != nil {
		return WebStatus{}, err
	}
	if !validWebStatus(result) {
		return WebStatus{}, ErrProtocol
	}
	return result, nil
}

func (client *OperatorClient) WebListClients(ctx context.Context, after string) (WebClientPage, error) {
	if after != "" && !validID(after) {
		return WebClientPage{}, ErrInvalidInput
	}
	params := struct {
		After string `json:"after"`
	}{After: after}
	var result WebClientPage
	if err := client.client.call(ctx, "web_list_clients", params, &result); err != nil {
		return WebClientPage{}, err
	}
	if !validWebClientPage(result) {
		return WebClientPage{}, ErrProtocol
	}
	return result, nil
}

func (client *OperatorClient) WebRevokeClient(ctx context.Context, id string, expectedRevision uint64) (WebRevokeResult, error) {
	if !validID(id) || expectedRevision == 0 {
		return WebRevokeResult{}, ErrInvalidInput
	}
	params := struct {
		ID               string `json:"id"`
		ExpectedRevision uint64 `json:"expected_revision"`
	}{ID: id, ExpectedRevision: expectedRevision}
	var result WebRevokeResult
	if err := client.client.call(ctx, "web_revoke_client", params, &result); err != nil {
		return WebRevokeResult{}, err
	}
	if !validWebRevokeResult(result) {
		return WebRevokeResult{}, ErrProtocol
	}
	return result, nil
}

func (client *OperatorClient) RemoteStatus(ctx context.Context) (RemoteStatus, error) {
	var result RemoteStatus
	if err := client.client.call(ctx, "remote_status", struct{}{}, &result); err != nil {
		return RemoteStatus{}, err
	}
	if !validRemoteStatus(result) {
		return RemoteStatus{}, ErrProtocol
	}
	return result, nil
}

func (client *OperatorClient) Snapshot(ctx context.Context) (DashboardSnapshot, error) {
	var result DashboardSnapshot
	if err := client.client.call(ctx, "snapshot", struct{}{}, &result); err != nil {
		return DashboardSnapshot{}, err
	}
	if !validSnapshot(result) {
		return DashboardSnapshot{}, ErrProtocol
	}
	return result, nil
}

func (client *OperatorClient) CreateProject(ctx context.Context, input CreateProjectInput) (MutationResult, error) {
	if !validID(input.ID) || !validText(input.Name, 1, 128) || !validText(input.Root, 1, 4096) {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.mutate(ctx, "create_project", input)
}

func (client *OperatorClient) CreateAgent(ctx context.Context, input CreateAgentInput) (MutationResult, error) {
	if !validCreateAgentInput(input) {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.mutate(ctx, "create_agent", input)
}

func (client *OperatorClient) EnqueueTask(ctx context.Context, input EnqueueTaskInput) (MutationResult, error) {
	if !validID(input.ID) || !validID(input.ProjectID) || !validID(input.AssignedAgentID) || !validID(input.IncarnationID) || !validText(input.Title, 1, 1024) || !validText(input.Body, 0, 131072) || input.Priority < -1_000_000 || input.Priority > 1_000_000 {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.mutate(ctx, "enqueue_task", input)
}

func (client *OperatorClient) SetDispatch(ctx context.Context, expectedRevision uint64, enabled bool) (MutationResult, error) {
	if expectedRevision == 0 {
		return MutationResult{}, ErrInvalidInput
	}
	params := struct {
		ExpectedRevision uint64 `json:"expected_revision"`
		Enabled          bool   `json:"enabled"`
	}{ExpectedRevision: expectedRevision, Enabled: enabled}
	return client.client.mutate(ctx, "set_dispatch", params)
}

func (client *AttemptClient) Succeed(ctx context.Context, result string) (MutationResult, error) {
	if !validText(result, 0, 131072) {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.mutate(ctx, "succeed", struct {
		Result string `json:"result"`
	}{Result: result})
}

func (client *AttemptClient) Task(ctx context.Context) (AttemptTask, error) {
	var result AttemptTask
	if err := client.client.call(ctx, "task", struct{}{}, &result); err != nil {
		return AttemptTask{}, err
	}
	if !validAttemptTask(result) {
		return AttemptTask{}, ErrProtocol
	}
	return result, nil
}

func (client *AttemptClient) Block(ctx context.Context, detail string) (MutationResult, error) {
	if !validText(detail, 1, 4096) {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.attemptDetail(ctx, "block", detail)
}

func (client *AttemptClient) Fail(ctx context.Context, detail string) (MutationResult, error) {
	if !validText(detail, 0, 4096) {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.attemptDetail(ctx, "fail", detail)
}

func (client *AttemptClient) RequestHuman(ctx context.Context, input HumanQuestionInput) (MutationResult, error) {
	if !validID(input.IdempotencyKey) || !validText(input.Question, 1, 8192) {
		return MutationResult{}, ErrInvalidInput
	}
	return client.client.mutate(ctx, "request_human", input)
}

func (client client) attemptDetail(ctx context.Context, method, detail string) (MutationResult, error) {
	return client.mutate(ctx, method, struct {
		Detail string `json:"detail"`
	}{Detail: detail})
}

func (client client) mutate(ctx context.Context, method string, params any) (MutationResult, error) {
	var result MutationResult
	if err := client.call(ctx, method, params, &result); err != nil {
		return MutationResult{}, err
	}
	if result.Revision == 0 {
		return MutationResult{}, ErrProtocol
	}
	return result, nil
}

type requestEnvelope struct {
	Method string `json:"method"`
	Params any    `json:"params"`
}

type responseEnvelope struct {
	OK    *bool           `json:"ok"`
	Data  json.RawMessage `json:"data,omitempty"`
	Error RemoteErrorCode `json:"error,omitempty"`
}

func (client client) call(ctx context.Context, method string, params, output any) error {
	current, err := loadToken(client.tokenPath)
	if err != nil || !current.same(client.token) {
		return ErrInvalidClient
	}
	encoded, err := json.Marshal(requestEnvelope{Method: method, Params: params})
	if err != nil || len(encoded)+requestPrelude > maxFrameBytes {
		return ErrInvalidInput
	}

	ctx, cancel := context.WithTimeout(ctx, requestTimeout)
	defer cancel()
	before, err := inspectSocket(client.socketPath)
	if err != nil || !before.same(client.socket) {
		return ErrInvalidClient
	}
	connection, err := (&net.Dialer{}).DialContext(ctx, "unix", client.socketPath)
	if err != nil {
		return classifyTransport(ctx)
	}
	defer connection.Close()
	if err := verifySocketConnection(connection, before); err != nil {
		return err
	}
	if err := setConnectionDeadline(connection, ctx); err != nil {
		return classifyTransport(ctx)
	}
	stop := watchCancellation(ctx, connection)
	defer stop()

	payload := make([]byte, requestPrelude+len(encoded))
	payload[0] = client.domain
	copy(payload[1:requestPrelude], current.bearer[:])
	copy(payload[requestPrelude:], encoded)
	if err := writeFrame(connection, payload); err != nil {
		return classifyTransport(ctx)
	}
	outcomeReceipt := client.domain == attemptDomain && outcomeMethod(method)
	unix, ok := connection.(*net.UnixConn)
	if !ok {
		return ErrTransport
	}
	if !outcomeReceipt && unix.CloseWrite() != nil {
		return ErrTransport
	}
	response, err := readFrame(connection)
	if err != nil {
		return classifyFrameError(ctx, err)
	}
	if len(response) < responsePrelude || response[0] != client.domain {
		return ErrProtocol
	}
	responseErr := decodeResponse(response[responsePrelude:], output)
	if outcomeReceipt && responseErr == nil {
		result, ok := output.(*MutationResult)
		if !ok || result.Revision == 0 {
			responseErr = ErrProtocol
		}
	}
	if outcomeReceipt && !acknowledgeableOutcomeResponse(responseErr) {
		if err := client.revalidate(before); err != nil {
			return err
		}
		return responseErr
	}
	if outcomeReceipt {
		receipt, err := readFrame(connection)
		if err != nil {
			// Rejections before the server accepts an outcome call have no
			// receipt. Once dispatch begins, every outcome response does.
			if responseErr == nil && errors.Is(err, io.EOF) {
				return ErrProtocol
			}
			if responseErr == nil || !errors.Is(err, io.EOF) {
				return classifyFrameError(ctx, err)
			}
		} else {
			if len(receipt) != outcomeReceiptBytes {
				return ErrProtocol
			}
			if err := requireEOF(connection); err != nil {
				return classifyFrameError(ctx, err)
			}
			if err := client.revalidate(before); err != nil {
				return err
			}
			if err := writeFrame(connection, receipt); err != nil {
				return classifyTransport(ctx)
			}
			clear(receipt)
			if err := unix.CloseWrite(); err != nil {
				return ErrTransport
			}
			return responseErr
		}
	}
	if err := requireEOF(connection); err != nil {
		return classifyFrameError(ctx, err)
	}
	if err := client.revalidate(before); err != nil {
		return err
	}
	return responseErr
}

func (client client) revalidate(before socketRecord) error {
	after, err := inspectSocket(client.socketPath)
	if err != nil || !after.same(before) {
		return ErrInvalidClient
	}
	latest, err := loadToken(client.tokenPath)
	if err != nil || !latest.same(client.token) {
		return ErrInvalidClient
	}
	return nil
}

func outcomeMethod(method string) bool {
	return method == "succeed" || method == "block" || method == "fail"
}

func acknowledgeableOutcomeResponse(err error) bool {
	if err == nil {
		return true
	}
	var remote *RemoteError
	return errors.As(err, &remote)
}

func writeFrame(writer io.Writer, payload []byte) error {
	if len(payload) == 0 || len(payload) > maxFrameBytes {
		return ErrProtocol
	}
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(payload)))
	if err := writeAll(writer, header[:]); err != nil {
		return err
	}
	return writeAll(writer, payload)
}

func writeAll(writer io.Writer, value []byte) error {
	for len(value) > 0 {
		written, err := writer.Write(value)
		if err != nil {
			return err
		}
		if written < 1 || written > len(value) {
			return io.ErrShortWrite
		}
		value = value[written:]
	}
	return nil
}

func readFrame(reader io.Reader) ([]byte, error) {
	var header [4]byte
	if _, err := io.ReadFull(reader, header[:]); err != nil {
		return nil, err
	}
	size := binary.BigEndian.Uint32(header[:])
	if size == 0 || size > maxFrameBytes {
		return nil, ErrProtocol
	}
	payload := make([]byte, int(size))
	if _, err := io.ReadFull(reader, payload); err != nil {
		return nil, err
	}
	return payload, nil
}

func requireEOF(reader io.Reader) error {
	var extra [1]byte
	count, err := reader.Read(extra[:])
	if count != 0 || err == nil {
		return ErrProtocol
	}
	if !errors.Is(err, io.EOF) {
		return err
	}
	return nil
}

func decodeResponse(encoded []byte, output any) error {
	var envelope responseEnvelope
	if err := decodeExact(encoded, &envelope); err != nil {
		return ErrProtocol
	}
	if envelope.OK == nil {
		return ErrProtocol
	}
	if *envelope.OK {
		if envelope.Error != "" || len(envelope.Data) == 0 || bytes.Equal(envelope.Data, []byte("null")) {
			return ErrProtocol
		}
		if err := decodeExact(envelope.Data, output); err != nil {
			return ErrProtocol
		}
		return nil
	}
	if len(envelope.Data) != 0 || !validRemoteCode(envelope.Error) {
		return ErrProtocol
	}
	return &RemoteError{code: envelope.Error}
}

// decodeExact reads one bounded JSON value with this protocol's exact grammar.
// A member neither side's build knows is ignored, so factoryctl, factoryd and
// factory-runner tolerate an added member instead of failing closed on it.
// Every bound, and every explicit required-member and value check made by the
// caller, still applies.
func decodeExact(encoded []byte, output any) error {
	if err := validateJSONNames(encoded); err != nil {
		return err
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	if err := decoder.Decode(output); err != nil {
		return err
	}
	if decoder.Decode(&struct{}{}) != io.EOF {
		return ErrProtocol
	}
	return nil
}

const maxJSONDepth = 64

// validateJSONNames rejects byte sequences encoding/json intentionally accepts
// but this exact local protocol does not: malformed UTF-8, unpaired UTF-16
// escapes, and duplicate object names. The frame bound caps total work and
// storage; maxJSONDepth caps stack.
func validateJSONNames(encoded []byte) error {
	if len(encoded) == 0 || len(encoded) > maxFrameBytes || !utf8.Valid(encoded) || !validJSONUnicodeEscapes(encoded) {
		return ErrProtocol
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	if err := validateJSONValue(decoder, 0); err != nil {
		return ErrProtocol
	}
	if _, err := decoder.Token(); !errors.Is(err, io.EOF) {
		return ErrProtocol
	}
	return nil
}

// validJSONUnicodeEscapes scans only JSON string boundaries and escapes. It
// prevents encoding/json from collapsing distinct lone surrogate escapes to
// U+FFFD; the ordinary decoder remains authority for all other JSON grammar.
func validJSONUnicodeEscapes(encoded []byte) bool {
	inString := false
	for index := 0; index < len(encoded); index++ {
		switch encoded[index] {
		case '"':
			inString = !inString
		case '\\':
			if !inString || index+1 >= len(encoded) {
				continue
			}
			if encoded[index+1] != 'u' {
				index++
				continue
			}
			unit, ok := jsonHexCodeUnit(encoded[index+2:])
			if !ok {
				return false
			}
			index += 5
			switch {
			case unit >= 0xdc00 && unit <= 0xdfff:
				return false
			case unit >= 0xd800 && unit <= 0xdbff:
				next := index + 1
				if next+6 > len(encoded) || encoded[next] != '\\' || encoded[next+1] != 'u' {
					return false
				}
				low, ok := jsonHexCodeUnit(encoded[next+2:])
				if !ok || low < 0xdc00 || low > 0xdfff {
					return false
				}
				index += 6
			}
		}
	}
	return true
}

func jsonHexCodeUnit(encoded []byte) (uint16, bool) {
	if len(encoded) < 4 {
		return 0, false
	}
	var value uint16
	for _, character := range encoded[:4] {
		value <<= 4
		switch {
		case character >= '0' && character <= '9':
			value |= uint16(character - '0')
		case character >= 'a' && character <= 'f':
			value |= uint16(character-'a') + 10
		case character >= 'A' && character <= 'F':
			value |= uint16(character-'A') + 10
		default:
			return 0, false
		}
	}
	return value, true
}

func validateJSONValue(decoder *json.Decoder, depth int) error {
	if depth > maxJSONDepth {
		return ErrProtocol
	}
	token, err := decoder.Token()
	if err != nil {
		return err
	}
	delimiter, compound := token.(json.Delim)
	if !compound {
		return nil
	}
	switch delimiter {
	case '{':
		names := make(map[string]struct{})
		for decoder.More() {
			nameToken, err := decoder.Token()
			if err != nil {
				return err
			}
			name, ok := nameToken.(string)
			if !ok || !canonicalJSONName(name) {
				return ErrProtocol
			}
			if _, duplicate := names[name]; duplicate {
				return ErrProtocol
			}
			names[name] = struct{}{}
			if err := validateJSONValue(decoder, depth+1); err != nil {
				return err
			}
		}
		end, err := decoder.Token()
		if err != nil || end != json.Delim('}') {
			return ErrProtocol
		}
	case '[':
		for decoder.More() {
			if err := validateJSONValue(decoder, depth+1); err != nil {
				return err
			}
		}
		end, err := decoder.Token()
		if err != nil || end != json.Delim(']') {
			return ErrProtocol
		}
	default:
		return ErrProtocol
	}
	return nil
}

func canonicalJSONName(name string) bool {
	if len(name) == 0 || name[0] < 'a' || name[0] > 'z' {
		return false
	}
	for index := 1; index < len(name); index++ {
		character := name[index]
		if character >= 'a' && character <= 'z' || character >= '0' && character <= '9' || character == '_' {
			continue
		}
		return false
	}
	return true
}

func validRemoteCode(code RemoteErrorCode) bool {
	switch code {
	case RemoteInvalidRequest, RemoteUnauthorized, RemoteForbidden, RemoteNotFound, RemoteConflict, RemoteRevisionConflict, RemoteTooLarge, RemoteUnavailable, RemoteCleanupUnresolved, RemoteInternal:
		return true
	default:
		return false
	}
}

func validID(value string) bool {
	if len(value) != 32 || value == strings.Repeat("0", 32) {
		return false
	}
	decoded := make([]byte, 16)
	_, err := hex.Decode(decoded, []byte(value))
	return err == nil && value == strings.ToLower(value)
}

func validText(value string, minimum, maximum int) bool {
	return utf8.ValidString(value) && !strings.ContainsRune(value, 0) && len(value) >= minimum && len(value) <= maximum
}

func validSnapshot(snapshot DashboardSnapshot) bool {
	if snapshot.Factory.Capacity < 1 || snapshot.Factory.Capacity > 1024 || snapshot.Factory.ActiveRuns > snapshot.Factory.Capacity || snapshot.Factory.Revision == 0 || snapshot.Projects == nil || snapshot.Agents == nil || snapshot.Tasks == nil || len(snapshot.Projects) > maxSnapshotEntries || len(snapshot.Agents) > maxSnapshotEntries || len(snapshot.Tasks) > maxSnapshotEntries {
		return false
	}
	for _, project := range snapshot.Projects {
		if !validID(project.ID) || !validText(project.Name, 1, 128) || project.Revision == 0 {
			return false
		}
	}
	for _, agent := range snapshot.Agents {
		if !validID(agent.ID) || !validID(agent.ProjectID) || !validText(agent.Name, 1, 128) || agent.Role != "worker" && agent.Role != "orchestrator" || !validProvider(agent.Provider) || agent.Revision == 0 {
			return false
		}
	}
	for _, task := range snapshot.Tasks {
		if !validID(task.ID) || !validID(task.ProjectID) || !validID(task.AssignedAgentID) || !validText(task.Title, 1, 1024) || !validTaskStatus(task.Status) || task.Priority < -1_000_000 || task.Priority > 1_000_000 || task.Revision == 0 {
			return false
		}
	}
	return true
}

func validProvider(value string) bool {
	return value == "shell" || value == "claude_code" || value == "codex"
}

func validWebStatus(status WebStatus) bool {
	if status.State != "ready" && status.State != "stopped" && status.State != "degraded" {
		return false
	}
	if status.State == "stopped" {
		return !status.Ready && status.Address == "" && status.Path == "" && status.Origins == nil && status.ActiveClients == 0 && status.RevokedClients == 0 && status.ActiveChallenges == 0
	}
	if status.Ready != (status.State == "ready") || !validText(status.Address, 1, 128) || !validText(status.Path, 1, 128) || len(status.Origins) == 0 || len(status.Origins) > 8 {
		return false
	}
	for _, origin := range status.Origins {
		if !validText(origin, 1, 4096) || strings.ContainsAny(origin, " \t\r\n*") {
			return false
		}
	}
	return true
}

func validRemoteStatus(status RemoteStatus) bool {
	if !validNodeID(status.NodeID) || !validText(status.RelayOrigin, 1, 4096) || strings.ContainsAny(status.RelayOrigin, " \t\r\n*") {
		return false
	}
	if !strings.HasPrefix(status.RelayOrigin, "wss://") && !strings.HasPrefix(status.RelayOrigin, "ws://") {
		return false
	}
	// A lost host connection closes every relayed session under one lock, so a
	// disconnected connector reporting live sessions is not a state the daemon
	// can occupy.
	return status.Sessions >= 0 && (status.Connected || status.Sessions == 0)
}

// validNodeID is the relay's self-certifying node id: exactly 32 characters of
// lowercase RFC 4648 base32 without padding.
func validNodeID(value string) bool {
	if len(value) != 32 {
		return false
	}
	for _, character := range value {
		if character >= 'a' && character <= 'z' || character >= '2' && character <= '7' {
			continue
		}
		return false
	}
	return true
}

func validDigest(value string) bool {
	if len(value) != 64 || value == strings.Repeat("0", 64) {
		return false
	}
	decoded := make([]byte, 32)
	_, err := hex.Decode(decoded, []byte(value))
	return err == nil && value == strings.ToLower(value)
}

func validWebClient(client WebClient) bool {
	if !validID(client.ID) || client.CapabilityMask == 0 || client.CapabilityMask&1 == 0 || client.CapabilityMask&^15 != 0 || client.Revision == 0 || client.UpdatedAtMs < client.CreatedAtMs {
		return false
	}
	return client.RevokedAtMs == nil || *client.RevokedAtMs >= client.CreatedAtMs && *client.RevokedAtMs <= client.UpdatedAtMs
}

func validWebClientPage(page WebClientPage) bool {
	if page.Clients == nil || len(page.Clients) > 128 {
		return false
	}
	seen := make(map[string]struct{}, len(page.Clients))
	for index, client := range page.Clients {
		if !validWebClient(client) {
			return false
		}
		if index > 0 {
			previous := page.Clients[index-1]
			if client.CreatedAtMs < previous.CreatedAtMs || client.CreatedAtMs == previous.CreatedAtMs && client.ID <= previous.ID {
				return false
			}
		}
		if _, ok := seen[client.ID]; ok {
			return false
		}
		seen[client.ID] = struct{}{}
	}
	if page.NextAfter != nil {
		if len(page.Clients) != 128 || !validID(*page.NextAfter) || *page.NextAfter != page.Clients[len(page.Clients)-1].ID {
			return false
		}
	}
	return true
}

func validWebRevokeResult(result WebRevokeResult) bool {
	return validID(result.ID) && result.Revision > 0
}

func validTaskStatus(status string) bool {
	switch status {
	case "queued", "running", "blocked", "succeeded", "failed", "cancelled":
		return true
	default:
		return false
	}
}

func classifyTransport(ctx context.Context) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	return ErrTransport
}

func classifyFrameError(ctx context.Context, err error) error {
	if ctxErr := ctx.Err(); ctxErr != nil {
		return ctxErr
	}
	var networkError net.Error
	if errors.As(err, &networkError) && networkError.Timeout() {
		return context.DeadlineExceeded
	}
	if errors.Is(err, ErrProtocol) {
		return ErrProtocol
	}
	return ErrTransport
}

func setConnectionDeadline(connection net.Conn, ctx context.Context) error {
	deadline, ok := ctx.Deadline()
	if !ok {
		return ErrTransport
	}
	return connection.SetDeadline(deadline)
}

type deadlineConnection interface {
	SetDeadline(time.Time) error
}

func watchCancellation(ctx context.Context, connection deadlineConnection) func() {
	finished := make(chan struct{})
	var wait sync.WaitGroup
	wait.Add(1)
	go func() {
		defer wait.Done()
		select {
		case <-ctx.Done():
			_ = connection.SetDeadline(time.Now())
		case <-finished:
		}
	}()
	return func() {
		close(finished)
		wait.Wait()
	}
}

func (left credential) equal(right credential) bool {
	return subtle.ConstantTimeCompare(left[:], right[:]) == 1
}
