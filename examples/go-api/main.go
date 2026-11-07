// A minimal HTTP service: / answers with the replica's hostname,
// /healthz is what mypaas probes before it sends traffic to a new replica.
package main

import (
	"fmt"
	"log"
	"net/http"
	"os"
)

func main() {
	host, _ := os.Hostname()
	http.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "ok")
	})
	http.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintf(w, "hello from %s\n", host)
	})
	log.Fatal(http.ListenAndServe(":8080", nil))
}
