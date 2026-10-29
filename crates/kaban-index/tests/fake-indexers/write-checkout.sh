#!/bin/sh
# Writes a file into the working directory (the checkout) outside the scratch (CG4.3
# negative path: in the `none` tier this succeeds and the quota does not see it).
echo "written by the indexer" > ./outside-scratch.txt && echo "wrote outside"
