package main

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"sync"
	"sync/atomic"
	"time"
)

type Server interface {
	Address() string
	IsAlive() bool
	Score() time.Duration
	Serve(http.ResponseWriter, *http.Request)
}

type simpleServer struct {
	address string
	proxy    *httputil.ReverseProxy
	client   *http.Client

	mu       sync.Mutex
	ewma     time.Duration
	inflight atomic.Int64
}

func newSimpleServer(addr string) *simpleServer {
	u, err := url.Parse(addr)
	handleErr(err)

	s := &simpleServer{
		address: addr,
		proxy:   httputil.NewSingleHostReverseProxy(u),
		client:   &http.Client{Timeout: 3 * time.Second},
		ewma:     100 * time.Millisecond,
	}

	s.proxy.ErrorHandler = func(rw http.ResponseWriter, r *http.Request, err error) {
		fmt.Printf("proxy error to %s: %v\n", s.address, err)
		http.Error(rw, "bad gateway", http.StatusBadGateway)
	}

	return s
}

func (s *simpleServer) Address() string { return s.address }

func (s *simpleServer) IsAlive() bool {
	resp, err := s.client.Get(s.address)
	if err != nil {
		return false
	}
	defer resp.Body.Close()
	return resp.StatusCode < 500
}

func (s *simpleServer) Score() time.Duration {
	s.mu.Lock()
	defer s.mu.Unlock()

	// Small penalty per concurrent request, so the scheduler avoids hot servers.
	return s.ewma + time.Duration(s.inflight.Load())*25*time.Millisecond
}

func (s *simpleServer) observe(d time.Duration) {
	const alpha = 0.2

	s.mu.Lock()
	defer s.mu.Unlock()

	if s.ewma <= 0 {
		s.ewma = d
		return
	}

	s.ewma = time.Duration(alpha*float64(d) + (1-alpha)*float64(s.ewma))
}

func (s *simpleServer) Serve(rw http.ResponseWriter, r *http.Request) {
	s.inflight.Add(1)
	start := time.Now()

	defer func() {
		s.inflight.Add(-1)
		s.observe(time.Since(start))
	}()

	s.proxy.ServeHTTP(rw, r)
}

type LoadBalancer struct {
	port    string
	servers []Server
}

func NewLoadBalancer(port string, servers []Server) *LoadBalancer {
	return &LoadBalancer{
		port:    port,
		servers: servers,
	}
}

func handleErr(err error) {
	if err != nil {
		fmt.Printf("Error: %v\n", err)
		os.Exit(1)
	}
}

func (lb *LoadBalancer) getBestServer() Server {
	var best Server
	bestScore := time.Duration(1<<63 - 1)

	for _, server := range lb.servers {
		if !server.IsAlive() {
			continue
		}

		score := server.Score()
		if best == nil || score < bestScore {
			best = server
			bestScore = score
		}
	}

	return best
}

func (lb *LoadBalancer) serveProxy(rw http.ResponseWriter, req *http.Request) {
	targetServer := lb.getBestServer()
	if targetServer == nil {
		http.Error(rw, "no healthy backends available", http.StatusServiceUnavailable)
		return
	}

	fmt.Printf("Forwarding request to: %q\n", targetServer.Address())
	targetServer.Serve(rw, req)
}

func loggingMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
		fmt.Printf("Received request: %s %s\n", r.Method, r.URL.Path)
		next.ServeHTTP(rw, r)
	})
}

func main() {
	servers := []Server{
		newSimpleServer("http://localhost:8001"),
		newSimpleServer("http://localhost:8002"),
		newSimpleServer("http://localhost:8003"),
	}

	lb := NewLoadBalancer("8000", servers)

	mux := http.NewServeMux()
	mux.HandleFunc("/", func(rw http.ResponseWriter, req *http.Request) {
		lb.serveProxy(rw, req)
	})

	srv := &http.Server{
		Addr:    ":8000",
		Handler: loggingMiddleware(mux),
	}

	go func() {
		fmt.Printf("Serving requests at localhost:%s\n", lb.port)
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			handleErr(err)
		}
	}()

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt)
	<-stop

	fmt.Println("\nShutting down the server...")

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	handleErr(srv.Shutdown(ctx))
	fmt.Println("Server gracefully stopped.")
}

// Author: Morteza Farrokhnejad