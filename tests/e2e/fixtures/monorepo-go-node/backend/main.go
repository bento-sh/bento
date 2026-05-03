package main

import "fmt"

func main() {
	fmt.Println(Response("backend"))
}

func Response(name string) string {
	return fmt.Sprintf("ok from %s", name)
}
