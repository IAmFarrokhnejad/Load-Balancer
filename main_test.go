package main

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestSimpleServerIsAlive(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, req *http.Request) {
		rw.WriteHeader(http.StatusOK) // Simulating an alive server
	}))
	defer server.Close()

	simpleServer := newSimpleServer(server.URL)
	if !simpleServer.IsAlive() {
		t.Errorf("Expected server to be alive")
	}
}

func TestLoadBalancer_getNextAvailableServer(t *testing.T) {
	aliveServer := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, req *http.Request) {
		rw.WriteHeader(http.StatusOK)
	}))
	defer aliveServer.Close()

	unavailableServer := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, req *http.Request) {
		rw.WriteHeader(http.StatusInternalServerError)
	}))
	defer unavailableServer.Close()

	servers := []Server{
		newSimpleServer(unavailableServer.URL),
		newSimpleServer(aliveServer.URL),
	}

	lb := NewLoadBalancer("8000", servers)

	// Expect load balancer to skip the unavailable server and use the alive one.
	if lb.getNextAvailableServer().Address() != aliveServer.URL {
		t.Errorf("Expected alive server to be selected")
	}
}

func TestLoadBalancer_ServeProxy(t *testing.T) {
	// Create a test server to act as the backend
	backendServer := httptest.NewServer(http.HandlerFunc(func(rw http.ResponseWriter, req *http.Request) {
		rw.WriteHeader(http.StatusOK)
	}))
	defer backendServer.Close()

	// Set up load balancer with this server
	lb := NewLoadBalancer("8000", []Server{newSimpleServer(backendServer.URL)})

	req := httptest.NewRequest("GET", "/", nil)
	rw := httptest.NewRecorder()

	lb.serveProxy(rw, req)

	// Expect the response to be forwarded and return a status OK
	if status := rw.Result().StatusCode; status != http.StatusOK {
		t.Errorf("Expected status OK; got %v", status)
	}
}