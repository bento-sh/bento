// Minimal .NET entry point for the bento e2e harness. Kept tiny on
// purpose: the harness asserts bento's init / ci / cache behaviour
// end-to-end, not C# the language.
Console.WriteLine(Greeter.Greeting("bento"));

internal static class Greeter
{
    internal static string Greeting(string name) => $"hello, {name}";
}
