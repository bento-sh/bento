import { greeting } from "./main.ts";

// Zero-import test — avoids `jsr:` / `https:` fetches so `deno test`
// succeeds offline. The bento e2e harness runs this on dev machines
// that may not have network access to jsr.io.
Deno.test("greeting is friendly", () => {
  const got = greeting("world");
  const want = "hello, world";
  if (got !== want) {
    throw new Error(`greeting("world") = ${got}, want ${want}`);
  }
});
