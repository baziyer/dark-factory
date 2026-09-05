package main

import (
	"context"
	"errors"
	"io"

	"github.com/dark-factory-build/dark-factory/internal/api"
)

func parseRemote(args []string) (attemptCommand, bool, bool) {
	if len(args) == 2 && helpFlag(args[1]) {
		return attemptCommand{}, true, true
	}
	if len(args) == 3 && helpFlag(args[2]) && args[1] == "status" {
		return attemptCommand{}, true, true
	}
	if len(args) == 2 && args[1] == "status" {
		return attemptCommand{kind: commandRemoteStatus}, false, true
	}
	return attemptCommand{}, false, false
}

func runRemote(ctx context.Context, getenv func(string) string, stdout, stderr io.Writer) int {
	socket, token := getenv("DARK_FACTORY_SOCKET"), getenv("DARK_FACTORY_OPERATOR_TOKEN_FILE")
	if socket == "" || token == "" {
		_, _ = io.WriteString(stderr, "factoryctl: remote client configuration is invalid\n")
		return exitFailure
	}
	client, err := api.NewOperatorClient(socket, token)
	if err != nil {
		_, _ = io.WriteString(stderr, "factoryctl: remote client configuration is invalid\n")
		return exitFailure
	}
	callContext, cancel := context.WithTimeout(ctx, attemptRequestTimeout)
	defer cancel()
	result, callErr := client.RemoteStatus(callContext)
	if callErr != nil {
		return writeRemoteFailure(stderr, "remote status", callErr)
	}
	return writeJSON(stdout, result)
}

func writeRemoteFailure(stderr io.Writer, subject string, err error) int {
	var remote *api.RemoteError
	if errors.As(err, &remote) && remote.Code() == api.RemoteNotFound {
		_, _ = io.WriteString(stderr, "factoryctl: remote access is not enabled; start factoryd with --relay-origin\n")
		return exitFailure
	}
	return writeWebFailure(stderr, subject, err)
}
