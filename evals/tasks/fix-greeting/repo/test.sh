#!/bin/sh
[ "$(sh greet.sh World)" = "Hello, World!" ] || { echo "greeting is wrong"; exit 1; }
