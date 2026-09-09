//go:build linux

package api

import (
	"net"
	"os"
	"syscall"
)

func verifyPeerEUID(connection net.Conn) error {
	unix, ok := connection.(*net.UnixConn)
	if !ok {
		return ErrInvalidClient
	}
	raw, err := unix.SyscallConn()
	if err != nil {
		return ErrInvalidClient
	}
	var credential *syscall.Ucred
	var socketErr error
	if err := raw.Control(func(fd uintptr) {
		credential, socketErr = syscall.GetsockoptUcred(int(fd), syscall.SOL_SOCKET, syscall.SO_PEERCRED)
	}); err != nil || socketErr != nil || credential == nil || credential.Uid != uint32(os.Geteuid()) {
		return ErrInvalidClient
	}
	return nil
}
