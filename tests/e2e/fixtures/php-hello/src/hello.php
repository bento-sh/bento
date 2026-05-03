<?php

declare(strict_types=1);

function greeting(string $name): string
{
    return "hello, {$name}";
}

if (realpath($_SERVER['SCRIPT_FILENAME']) === __FILE__) {
    echo greeting('bento'), "\n";
}
