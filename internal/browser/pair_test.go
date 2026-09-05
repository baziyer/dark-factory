package browser

import (
	"context"
	"errors"
	"io"
	"net/http"
	"strings"
	"sync"
	"testing"
)

const pairLink = testOrigin + "/#df_pair=" + "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f"

type pairLinkBackend struct {
	*fakeBackend
	mu    sync.Mutex
	link  string
	err   error
	calls int
}

func (backend *pairLinkBackend) PairLink(context.Context) (string, error) {
	backend.mu.Lock()
	defer backend.mu.Unlock()
	backend.calls++
	if backend.link != "" {
		return backend.link, backend.err
	}
	return pairLink, backend.err
}

func (backend *pairLinkBackend) mints() int {
	backend.mu.Lock()
	defer backend.mu.Unlock()
	return backend.calls
}

// TestPairPageAdmitsOnlyUserNavigationsAndItsOwnForm sends the exact header
// shapes browsers send: a typed or OS-launched navigation, a link click from
// the console, the page's own form post, and every rejected shape (fetch,
// frame, cross-site post, scripted navigation from elsewhere, wrong referrer,
// wrong host, no metadata at all). Only the two admitted shapes answer, and
// only the form post mints.
func TestPairPageAdmitsOnlyUserNavigationsAndItsOwnForm(t *testing.T) {
	backend := &pairLinkBackend{fakeBackend: newFakeBackend()}
	server := startTaskServer(t, backend)
	client := &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	navigation := map[string]string{"Sec-Fetch-Mode": "navigate", "Sec-Fetch-Dest": "document"}
	with := func(extra ...string) map[string]string {
		result := make(map[string]string, len(navigation)+len(extra)/2)
		for key, value := range navigation {
			result[key] = value
		}
		for index := 0; index+1 < len(extra); index += 2 {
			result[extra[index]] = extra[index+1]
		}
		return result
	}
	tests := []struct {
		name    string
		method  string
		path    string
		host    string
		headers map[string]string
		status  int
		mints   int
	}{
		{"typed navigation", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "none", "Sec-Fetch-User", "?1"), http.StatusOK, 0},
		{"typed navigation without Sec-Fetch-User (Safari)", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "none"), http.StatusOK, 0},
		{"link from the console origin", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "cross-site", "Referer", testOrigin+"/"), http.StatusOK, 0},
		{"link from the development origin", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "cross-site", "Referer", devOrigin+"/factory"), http.StatusOK, 0},
		{"link from another origin", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "cross-site", "Referer", "https://evil.example/"), http.StatusNotFound, 0},
		{"referrer with userinfo", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "cross-site", "Referer", "https://app.darkfactory.build@evil.example/"), http.StatusNotFound, 0},
		{"scripted navigation without referrer", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "cross-site"), http.StatusNotFound, 0},
		{"same-site navigation without referrer", http.MethodGet, PairPath, "", with("Sec-Fetch-Site", "same-site"), http.StatusNotFound, 0},
		{"fetch", http.MethodGet, PairPath, "", map[string]string{"Sec-Fetch-Mode": "cors", "Sec-Fetch-Dest": "empty", "Sec-Fetch-Site": "none"}, http.StatusNotFound, 0},
		{"no-cors fetch", http.MethodGet, PairPath, "", map[string]string{"Sec-Fetch-Mode": "no-cors", "Sec-Fetch-Dest": "empty", "Sec-Fetch-Site": "cross-site"}, http.StatusNotFound, 0},
		{"frame", http.MethodGet, PairPath, "", map[string]string{"Sec-Fetch-Mode": "navigate", "Sec-Fetch-Dest": "iframe", "Sec-Fetch-Site": "cross-site", "Referer": testOrigin + "/"}, http.StatusNotFound, 0},
		{"no metadata", http.MethodGet, PairPath, "", nil, http.StatusNotFound, 0},
		{"wrong host", http.MethodGet, PairPath, "evil.example", with("Sec-Fetch-Site", "none"), http.StatusNotFound, 0},
		{"query string", http.MethodGet, PairPath + "?x=1", "", with("Sec-Fetch-Site", "none"), http.StatusNotFound, 0},
		{"escaped path", http.MethodGet, "/%70air", "", with("Sec-Fetch-Site", "none"), http.StatusNotFound, 0},
		{"head", http.MethodHead, PairPath, "", with("Sec-Fetch-Site", "none"), http.StatusNotFound, 0},
		{"own form post", http.MethodPost, PairPath, "", with("Sec-Fetch-Site", "same-origin", "Sec-Fetch-User", "?1"), http.StatusSeeOther, 1},
		{"own form post without Sec-Fetch-User (Safari)", http.MethodPost, PairPath, "", with("Sec-Fetch-Site", "same-origin"), http.StatusSeeOther, 1},
		{"cross-site form post", http.MethodPost, PairPath, "", with("Sec-Fetch-Site", "cross-site", "Referer", testOrigin+"/"), http.StatusNotFound, 0},
		{"same-site form post", http.MethodPost, PairPath, "", with("Sec-Fetch-Site", "same-site"), http.StatusNotFound, 0},
		{"post without site", http.MethodPost, PairPath, "", with("Sec-Fetch-Site", "none"), http.StatusNotFound, 0},
		{"posted by fetch", http.MethodPost, PairPath, "", map[string]string{"Sec-Fetch-Mode": "cors", "Sec-Fetch-Dest": "empty", "Sec-Fetch-Site": "same-origin"}, http.StatusNotFound, 0},
		{"post without metadata", http.MethodPost, PairPath, "", nil, http.StatusNotFound, 0},
		{"post to the wrong host", http.MethodPost, PairPath, "evil.example", with("Sec-Fetch-Site", "same-origin"), http.StatusNotFound, 0},
		{"put", http.MethodPut, PairPath, "", with("Sec-Fetch-Site", "same-origin"), http.StatusNotFound, 0},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			before := backend.mints()
			request, err := http.NewRequest(test.method, "http://"+server.Addr()+test.path, strings.NewReader(""))
			if err != nil {
				t.Fatal(err)
			}
			if test.host != "" {
				request.Host = test.host
			}
			for key, value := range test.headers {
				request.Header.Set(key, value)
			}
			response, err := client.Do(request)
			if err != nil {
				t.Fatal(err)
			}
			body, _ := io.ReadAll(response.Body)
			_ = response.Body.Close()
			if response.StatusCode != test.status {
				t.Fatalf("status=%d want %d body=%q", response.StatusCode, test.status, body)
			}
			if got := backend.mints() - before; got != test.mints {
				t.Fatalf("mints=%d want %d", got, test.mints)
			}
			switch test.status {
			case http.StatusOK:
				if !strings.Contains(string(body), "<form method=post action="+PairPath+">") || strings.Contains(string(body), "<script") {
					t.Fatalf("page=%q", body)
				}
				for key, want := range map[string]string{
					"Content-Type":            "text/html; charset=utf-8",
					"Content-Security-Policy": server.pairPolicy,
					"Cache-Control":           "no-store",
					"Referrer-Policy":         "no-referrer",
					"X-Content-Type-Options":  "nosniff",
					"X-Frame-Options":         "DENY",
				} {
					if got := response.Header.Get(key); got != want {
						t.Fatalf("%s=%q want %q", key, got, want)
					}
				}
				if !strings.HasPrefix(server.pairPolicy, "default-src 'none'; style-src 'sha256-") || !strings.HasSuffix(server.pairPolicy, "'; form-action 'self' "+devOrigin+" "+testOrigin+"; base-uri 'none'; frame-ancestors 'none'") {
					t.Fatalf("policy=%q", server.pairPolicy)
				}
			case http.StatusSeeOther:
				if got := response.Header.Get("Location"); got != pairLink {
					t.Fatalf("location=%q", got)
				}
				if strings.Contains(string(body), "df_pair") {
					t.Fatalf("redirect body leaks the link: %q", body)
				}
				if got := response.Header.Get("Cache-Control"); got != "no-store" {
					t.Fatalf("cache-control=%q", got)
				}
			default:
				if strings.Contains(string(body), "df_pair") || strings.Contains(string(body), "<form") {
					t.Fatalf("refusal leaks: %q", body)
				}
			}
		})
	}
}

func TestPairPageIsAbsentWithoutABackendAndFailsClosedOnMintErrors(t *testing.T) {
	client := &http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	post := func(t *testing.T, address string) *http.Response {
		t.Helper()
		request, err := http.NewRequest(http.MethodPost, "http://"+address+PairPath, strings.NewReader(""))
		if err != nil {
			t.Fatal(err)
		}
		for key, value := range map[string]string{"Sec-Fetch-Mode": "navigate", "Sec-Fetch-Dest": "document", "Sec-Fetch-Site": "same-origin"} {
			request.Header.Set(key, value)
		}
		response, err := client.Do(request)
		if err != nil {
			t.Fatal(err)
		}
		_ = response.Body.Close()
		return response
	}
	// A mint that fails, and one that answers with anything but a challenge
	// for an allowed origin, both fail the request rather than redirecting.
	for _, test := range []struct {
		name    string
		backend *pairLinkBackend
	}{
		{name: "mint error", backend: &pairLinkBackend{fakeBackend: newFakeBackend(), err: errors.New("mint failed")}},
		{name: "link outside the allowed origins", backend: &pairLinkBackend{fakeBackend: newFakeBackend(), link: "https://evil.example/#df_pair=" + strings.Repeat("a", 64)}},
	} {
		t.Run(test.name, func(t *testing.T) {
			server := startTaskServer(t, test.backend)
			if response := post(t, server.Addr()); response.StatusCode != http.StatusServiceUnavailable || response.Header.Get("Location") != "" {
				t.Fatalf("status=%d location=%q", response.StatusCode, response.Header.Get("Location"))
			}
		})
	}
	plain := startServer(t, newFakeBackend())
	if response := post(t, plain.Addr()); response.StatusCode != http.StatusNotFound {
		t.Fatalf("no pair backend status=%d", response.StatusCode)
	}
	request, _ := http.NewRequest(http.MethodGet, "http://"+plain.Addr()+PairPath, nil)
	for key, value := range map[string]string{"Sec-Fetch-Mode": "navigate", "Sec-Fetch-Dest": "document", "Sec-Fetch-Site": "none"} {
		request.Header.Set(key, value)
	}
	response, err := client.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusNotFound {
		t.Fatalf("no pair backend page status=%d", response.StatusCode)
	}
}
