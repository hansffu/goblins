#!/usr/bin/env bash
cd /home/hansffu/code/goblins
nix develop -c just test-merge > "$1" 2>&1; rc=$?
echo "exit=$rc"
grep -E "PASS \(|FAIL \(|^Ran |OK \(|FAILED|^\[FAIL\]" "$1" | tail -20
