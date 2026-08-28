#!/bin/sh
if ping -c1 -W1 1.1.1.1 >/dev/null 2>&1; then echo "●"; else echo "○"; fi
