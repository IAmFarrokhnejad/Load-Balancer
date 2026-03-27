package main

import (
	"context"
	"crypto/sha1"
	"encoding/binary"
	"fmt"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"sort"
	"strings"
	"sync"
	"time"
)

// Author: Morteza Farrokhnejad

type Server interface {
	Address() string
	IsAlive() bool
	Serve(http.ResponseWriter, *http.Request)
}

type simpleServer struct {
	address string
	proxy   *httputil.ReverseProxy
	client  *http.Client
}

func newSimpleServer(addr string) *simpleServer {
	u, err := url.Parse(addr)
	handleErr(err)

	s := &simpleServer{
		address: addr,
		proxy:   httputil.NewSingleHostReverseProxy(u),
		client:   &http.Client{Timeout: 3 * time.Second},
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

func (s *simpleServer) Serve(rw http.ResponseWriter, r *http.Request) {
	s.proxy.ServeHTTP(rw, r)
}

type ringNode struct {
	hash uint32
	idx  int
}

type LoadBalancer struct {
	port     string
	servers  []Server
	replicas int

	mu   sync.RWMutex
	ring []ringNode
}

func NewLoadBalancer(port string, servers []Server) *LoadBalancer {
	lb := &LoadBalancer{
		port:     port,
		servers:  servers,
		replicas: 100,
	}
	lb.buildRing()
	return lb
}

func handleErr(err error) {
	if err != nil {
		fmt.Printf("Error: %v\n", err)
		os.Exit(1)
	}
}

func hash32(key string) uint32 {
	sum := sha1.Sum([]byte(key))
	return binary.BigEndian.Uint32(sum[:4])
}

func (lb *LoadBalancer) buildRing() {
	lb.mu.Lock()
	defer lb.mu.Unlock()

	var ring []ringNode
	for i, srv := range lb.servers {
		base := srv.Address()
		for r := 0; r < lb.replicas; r++ {
			h := hash32(fmt.Sprintf("%s#%d", base, r))
			ring = append(ring, ringNode{hash: h, idx: i})
		}
	}

	sort.Slice(ring, func(i, j int) bool { return ring[i].hash < ring[j].hash })
	lb.ring = ring
}

func clientIP(r *http.Request) string {
	xff := r.Header.Get("X-Forwarded-For")
	if xff != "" {
		parts := strings.Split(xff, ",")
		return strings.TrimSpace(parts[0])
	}

	host, _, err := net.SplitHostPort(r.RemoteAddr)
	if err == nil {
		return host
	}
	return r.RemoteAddr
}

func (lb *LoadBalancer) getServerForClient(ip string) Server {
	lb.mu.RLock()
	defer lb.mu.RUnlock()

	if len(lb.ring) == 0 || len(lb.servers) == 0 {
		return nil
	}

	key := hash32(ip)
	pos := sort.Search(len(lb.ring), func(i int) bool {
		return lb.ring[i].hash >= key
	})
	if pos == len(lb.ring) {
		pos = 0
	}

	// Walk clockwise until an alive backend is found.
	for i := 0; i < len(lb.ring); i++ {
		node := lb.ring[(pos+i)%len(lb.ring)]
		srv := lb.servers[node.idx]
		if srv != nil && srv.IsAlive() {
			return srv
		}
	}

	return nil
}

func (lb *LoadBalancer) serveProxy(rw http.ResponseWriter, req *http.Request) {
	ip := clientIP(req)
	targetServer := lb.getServerForClient(ip)
	if targetServer == nil {
		http.Error(rw, "no healthy backends available", http.StatusServiceUnavailable)
		return
	}

	fmt.Printf("Client %s -> %s\n", ip, targetServer.Address())
	targetServer.Serve(rw, req)
}

func loggingMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
		fmt.Printf("Received request: %s %s from %s\n", r.Method, r.URL.Path, r.RemoteAddr)
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