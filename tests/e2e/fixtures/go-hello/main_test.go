package main

import "testing"

func TestGreeting(t *testing.T) {
	got := Greeting("world")
	want := "hello, world"
	if got != want {
		t.Fatalf("Greeting(%q) = %q, want %q", "world", got, want)
	}
}
