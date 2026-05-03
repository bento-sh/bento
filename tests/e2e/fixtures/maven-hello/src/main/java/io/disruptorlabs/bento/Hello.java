package io.disruptorlabs.bento;

public class Hello {
    public static String greeting(String name) {
        return "hello, " + name;
    }

    public static void main(String[] args) {
        System.out.println(greeting("bento"));
    }
}
