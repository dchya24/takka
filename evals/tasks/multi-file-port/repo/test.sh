#!/bin/sh
grep -q 9090 config.env && sh client.sh | grep -q 9090 && ! grep -q 8080 config.env client.sh server.sh
