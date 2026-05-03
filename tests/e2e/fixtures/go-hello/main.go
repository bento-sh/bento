// Minimal Go entry point for the bento e2e harness. Kept tiny on
// purpose: the harness asserts bento's init / ci / cache behaviour
// end-to-end, not Go the language.
package main

import "fmt"

func main() {
	fmt.Println(Greeting("bento"))
}

// Greeting builds a canonical hello string. The separate function
// gives `go test` something non-trivial to exercise.
func Greeting(name string) string {
	return fmt.Sprintf("hello, %s", name)
}
