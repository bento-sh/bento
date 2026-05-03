package main

import "testing"

func TestResponse(t *testing.T) {
	got := Response("x")
	if got != "ok from x" {
		t.Fatalf("Response(x) = %q, want %q", got, "ok from x")
	}
}
