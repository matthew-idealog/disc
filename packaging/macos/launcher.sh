#!/bin/bash
# DISC is a terminal app, so the .app opens it in a Terminal window.
DIR="$(cd "$(dirname "$0")" && pwd)"
open -a Terminal "$DIR/disc"
