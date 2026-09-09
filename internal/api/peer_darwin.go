//go:build darwin

package api

import (
	"net"
	"os"
	"runtime"
	"syscall"
	"unsafe"
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
	var peer darwinPeerCredential
	var socketErr syscall.Errno
	if err := raw.Control(func(fd uintptr) {
		length := uint32(unsafe.Sizeof(peer))
		_, _, socketErr = syscall.Syscall6(syscall.SYS_GETSOCKOPT, fd, darwinSOLLocal, darwinLocalPeerCred, uintptr(unsafe.Pointer(&peer)), uintptr(unsafe.Pointer(&length)), 0)
		if socketErr == 0 && length != uint32(unsafe.Sizeof(peer)) {
			socketErr = syscall.EINVAL
		}
		runtime.KeepAlive(&peer)
	}); err != nil || socketErr != 0 || peer.version != 0 || peer.uid != uint32(os.Geteuid()) || peer.groupCount < 0 || int(peer.groupCount) > len(peer.groups) {
		return ErrInvalidClient
	}
	return nil
}

const (
	darwinSOLLocal      = 0
	darwinLocalPeerCred = 1
)

// darwinPeerCredential is Darwin's fixed external xucred representation.
type darwinPeerCredential struct {
	version    uint32
	uid        uint32
	groupCount int16
	_          [2]byte
	groups     [16]uint32
}
