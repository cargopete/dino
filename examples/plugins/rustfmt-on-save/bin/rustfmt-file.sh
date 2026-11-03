#!/bin/sh
# Through standard input, because `rustfmt <file>` follows `mod` declarations
# and would reformat files the model never touched.
path=$1
edition=$2
rustfmt --edition "$edition" --emit stdout < "$path" > "$path.fmt" && mv "$path.fmt" "$path"
status=$?
rm -f "$path.fmt"
exit $status
