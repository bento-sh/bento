export function greeting(name: string): string {
  return `hello, ${name}`;
}

if (import.meta.main) {
  console.log(greeting("bento"));
}
