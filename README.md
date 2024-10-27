# Simple Load Balancer with Reverse Proxy and Graceful Shutdown

This Go application implements a simple HTTP load balancer with round-robin load distribution, reverse proxying, logging middleware, and a graceful shutdown mechanism.

## Features

- **Round-Robin Load Balancing**: Distributes incoming HTTP requests across multiple backend servers in a round-robin manner.
- **Health Checks**: Verifies server availability using `HEAD` requests to ensure only healthy servers receive traffic.
- **Reverse Proxying**: Uses `httputil.ReverseProxy` to forward incoming requests to backend servers.
- **Request Logging**: Logs each incoming request's HTTP method and path for easy monitoring.
- **Graceful Shutdown**: Ensures a clean shutdown by listening for interrupt signals and allowing ongoing requests to complete.

## Components

### `simpleServer`
Represents a server instance that forwards incoming requests to a specified backend server. It performs health checks and proxies requests using `httputil.ReverseProxy`.

#### Methods
- `Address()`: Returns the server's address.
- `IsAlive()`: Checks server health by sending a `HEAD` request.
- `Serve()`: Forwards requests to the backend server.

### `LoadBalancer`
Manages a list of servers, selecting a healthy server in round-robin fashion for each incoming request.

#### Methods
- `getNextAvailableServer()`: Returns the next available and healthy server.
- `serveProxy()`: Selects a server and forwards the request to it.

### Middleware
- **Logging Middleware**: Logs each request to standard output.

## Usage

1. **Define Servers**: Specify backend server URLs by creating `simpleServer` instances.
2. **Initialize Load Balancer**: Instantiate a `LoadBalancer` with a list of servers.
3. **Run Server**: Start the HTTP server on the specified port (`8000` by default) with graceful shutdown support.

## Graceful Shutdown
The server listens for an interrupt signal (e.g., `Ctrl+C`) and initiates a shutdown sequence that waits up to 5 seconds for in-progress requests to complete.

## Code Example

```go
package main

// Main entry point of the application
func main() {
    // Define backend servers
    servers := []Server{
        newSimpleServer("https://www.example.com"),
        newSimpleServer("https://www.bing.com"),
        newSimpleServer("https://www.google.com"),
    }

    // Initialize load balancer
    lb := NewLoadBalancer("8000", servers)

    // Define HTTP handler
    handleRedirect := func(rw http.ResponseWriter, req *http.Request) {
        lb.serveProxy(rw, req)
    }

    // Setup server and graceful shutdown
    mux := http.NewServeMux()
    mux.HandleFunc("/", handleRedirect)
    loggedMux := loggingMiddleware(mux)

    srv := &http.Server{
        Addr:    ":8000",
        Handler: loggedMux,
    }

    // Start server
    go func() {
        fmt.Printf("Serving requests at 'localhost:%s'\n", lb.port)
        if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
            handleErr(err)
        }
    }()

    // Graceful shutdown logic
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
