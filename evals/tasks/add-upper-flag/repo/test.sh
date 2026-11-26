#!/bin/sh
[ "$(sh shout.sh hi)" = hi ] && [ "$(sh shout.sh --upper hi)" = HI ]
