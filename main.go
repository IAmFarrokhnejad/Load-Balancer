package main

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/signal"
	"time"
)

type simpleServer struct {
	address string
	proxy   *httputil.ReverseProxy
}

func newSimpleServer(addr string) *simpleServer {
	serverUrl, err := url.Parse(addr)
	handleErr(err)

	return &simpleServer{
		address: addr,
		proxy:   httputil.NewSingleHostReverseProxy(serverUrl),
	}
}

type LoadBalancer struct {
	port            string
	roundRobinCount int
	servers         []Server
}

type Server interface {
	Address() string
	IsAlive() bool
	Serve(rw http.ResponseWriter, r *http.Request)
}

func NewLoadBalancer(port string, servers []Server) *LoadBalancer {
	return &LoadBalancer{
		port:            port,
		roundRobinCount: 0,
		servers:         servers,
	}
}

func handleErr(err error) {
	if err != nil {
		fmt.Printf("Error: %v\n", err)
		os.Exit(1)
	}
}

func (s *simpleServer) Address() string {
	return s.address
}

// Health check for server using a HEAD request to check if server is alive.
func (s *simpleServer) IsAlive() bool {
	resp, err := http.Head(s.address)
	if err != nil || resp.Status >= 400 {
		return false
	}
	return true
}

func (s *simpleServer) Serve(rw http.ResponseWriter, r *http.Request) {
	s.proxy.ServeHTTP(rw, r)
}

func (lb *LoadBalancer) getNextAvailableServer() Server {
	server := lb.servers[lb.roundRobinCount%len(lb.servers)]
	for !server.IsAlive() {
		lb.roundRobinCount++
		server = lb.servers[lb.roundRobinCount%len(lb.servers)]
	}

	lb.roundRobinCount--
	return server
}

func (lb *LoadBalancer) serveProxy(rw http.ResponseWriter, req *http.Request) {
	targetServer := lb.getNextAvailableServer()
	fmt.Printf("Forwarding the request to address: %q\n", targetServer.IsAlive())
	targetServer.Serve(rw, req)
}

// Middleware to log incoming requests
func loggingMiddleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(rw http.ResponseWriter, r *http.Request) {
		fmt.Printf("Received request: %s %s\n", r.Method, r.URL.Path)
		next.ServeHTTP(rw, r)
	})
}

func main() {
	servers := []Server{
		newSimpleServer("https://www.example.com"),
		newSimpleServer("https://www.bing.com"),
		newSimpleServer("https://www.google.com"),
	}

	lb := NewLoadBalancer("8000", servers)

	handleRedirect := func(rw http.ResponseWriter, req *http.Request) {
		lb.serveProxy(rw, req)
	}

	mux := http.NewServeMux()
	mux.HandleFunc("/", handleRedirect)

	// Apply logging middleware
	loggedMux := loggingMiddleware(mux)

	srv := &http.Server{
		Addr:    ":8000",
		Handler: loggedMux,
	}

	// Graceful shutdown
	go func() {
		fmt.Printf("Serving requests at 'localhost:%s'\n", lb.port)
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			handleErr(err)
		}
	}()

	// Capture interrupt signal to gracefully shutdown the server
	stop := make(chan os.Signal, 1)
	signal.Notify(stop, os.Interrupt)

	<-stop
	fmt.Println("\nShutting down the server...")

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := srv.Shutdown(ctx); err != nil {
		fmt.Printf("Shutdown error: %v\n", err)
	} else {
		fmt.Println("Server gracefully stopped.")
	}
}
//Author: Morteza Farrokhnejad