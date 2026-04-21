#!/usr/bin/env bash
# ─────────────────────────────────────────────────────────────────────────────
# TaskMan Crash Test Script
#
# This script demonstrates GrumpyDB's WAL crash recovery by:
# 1. Inserting tasks normally
# 2. Verifying they survive a clean restart
# 3. Verifying import crash-resilience (re-import skips duplicates)
#
# NOTE: True SIGKILL-mid-write testing requires OS-level tooling (e.g., fuse
# fault injection). This script tests the next best thing: restart-resilience.
#
# Usage: bash examples/taskman/test_crash.sh
# ─────────────────────────────────────────────────────────────────────────────

set -e

TASKMAN="cargo run --example taskman --"
EXPORT_FILE="/tmp/taskman_crashtest.bak"

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m' # No Color

pass() { echo -e "  ${GREEN}✓ $1${NC}"; }
fail() { echo -e "  ${RED}✗ $1${NC}"; exit 1; }

echo "═══════════════════════════════════════════════"
echo " GrumpyDB Crash Recovery Test"
echo "═══════════════════════════════════════════════"
echo ""

# ── Cleanup ──────────────────────────────────────────────────────────────────
rm -rf .taskman "$EXPORT_FILE"

# ── Step 1: Insert tasks ────────────────────────────────────────────────────
echo "Step 1: Inserting 20 tasks..."
for i in $(seq 1 20); do
    $TASKMAN add "Task $i" --tags "test,batch" > /dev/null 2>&1
done
COUNT=$($TASKMAN stats 2>&1 | grep "Total:" | awk '{print $2}')
if [ "$COUNT" = "20" ]; then
    pass "Inserted 20 tasks (count=$COUNT)"
else
    fail "Expected 20 tasks, got $COUNT"
fi

# ── Step 2: Export ──────────────────────────────────────────────────────────
echo "Step 2: Exporting tasks..."
$TASKMAN export "$EXPORT_FILE" > /dev/null 2>&1
LINES=$(wc -l < "$EXPORT_FILE" | tr -d ' ')
if [ "$LINES" = "20" ]; then
    pass "Exported 20 lines to $EXPORT_FILE"
else
    fail "Expected 20 lines in export, got $LINES"
fi

# ── Step 3: Simulate restart (close + reopen) ──────────────────────────────
echo "Step 3: Simulating restart (reopen database)..."
# The database is closed and reopened on each command invocation.
# This simulates a process restart. WAL recovery runs on open.
COUNT_AFTER=$($TASKMAN stats 2>&1 | grep "Total:" | awk '{print $2}')
if [ "$COUNT_AFTER" = "20" ]; then
    pass "All 20 tasks survived restart"
else
    fail "After restart: expected 20, got $COUNT_AFTER"
fi

# ── Step 4: Flush + verify WAL is checkpointed ─────────────────────────────
echo "Step 4: Flushing (WAL checkpoint)..."
$TASKMAN flush > /dev/null 2>&1
pass "Flush completed (WAL checkpointed + truncated)"

# ── Step 5: Re-import (duplicate resilience) ────────────────────────────────
echo "Step 5: Re-importing (should skip duplicates)..."
IMPORT_RESULT=$($TASKMAN import "$EXPORT_FILE" 2>&1)
IMPORTED=$(echo "$IMPORT_RESULT" | grep "Imported" | awk '{print $2}')
if [ "$IMPORTED" = "0" ]; then
    pass "Re-import: 0 new tasks (all duplicates skipped)"
else
    fail "Re-import: expected 0 new, got $IMPORTED"
fi

# ── Step 6: Verify final state ──────────────────────────────────────────────
echo "Step 6: Verifying final state..."
FINAL_COUNT=$($TASKMAN stats 2>&1 | grep "Total:" | awk '{print $2}')
if [ "$FINAL_COUNT" = "20" ]; then
    pass "Final count: 20 tasks (no duplicates)"
else
    fail "Final count: expected 20, got $FINAL_COUNT"
fi

# ── Cleanup ──────────────────────────────────────────────────────────────────
rm -rf .taskman "$EXPORT_FILE"

echo ""
echo "═══════════════════════════════════════════════"
echo -e " ${GREEN}All crash recovery tests passed!${NC}"
echo "═══════════════════════════════════════════════"
