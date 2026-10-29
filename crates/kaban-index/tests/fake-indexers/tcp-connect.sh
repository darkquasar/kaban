#!/bin/sh
# Attempts a TCP connection (CG4.3 reserves the name for the namespaces tier: in the `none`
# tier the network is the user's; the result is reported, never asserted as refused).
if command -v nc >/dev/null 2>&1; then nc -z -w 1 127.0.0.1 9 && echo connected || echo refused; else echo "nc absent"; fi
