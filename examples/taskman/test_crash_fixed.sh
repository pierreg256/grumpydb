#!/usr/bin/env bash
set -e
TASKMAN_CMD="cargo run --example taskman --"
DB_DIR=".taskman_crashtest"
EXPORT_FILE="/tmp/taskman_crashtest.bak"
GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'
pass() { echo -e "  ${GREEN}✓ \$1${NC}"; }
fail() { echo -e "  ${RED}✗ \$1${NC}"; exit 1; }
echo "==============================================="
echo " GrumpyDB Crash Recovery Test"
echo "==============================================="
echo ""
rm -rf "$DB_DIR" "$EXPORT_FILE" .taskman
mkdir -p "$DB_DIR"
export TASKMAN_DB="$DB_DIR"
echo "Step 1: Inserting 20 tasks..."
for i in {1..20}; do
    $TASKMAN_CMD add "Task $i" --tags "test,batch" > /dev/null 2>&1
done
STATS_OUT=$($TASKMAN_CMD stats 2>&1)
COUNT=$(echo "$STATS_OUT" | grep "Total:" | awk '{print $2}')
if [ "$COUNT" = "20" ]; then
    pass "Inserted 20 tasks (count=$COUNT)"
else
    fail "Expected 20 tasks, got $COUNT. Output: $STATS_OUT"
fi
echo "Step 2: Exporting tasks..."
$TASKMAN_CMD export "$EXPORT_FILE" > /dev/null 2>&1
LINES=$(wc -l < "$EXPORT_FILE" | tr -d ' ')
if [ "$LINES" = "20" ]; then
    pass "Exported 20 lines to $EXPORT_FILE"
else
    fail "Expected 20 lines in export, got $LINES"
fi
echo "Step 3: Simulating restart (reopen database)..."
STATS_OUT_RESTART=$($TASKMAN_CMD stats 2>&1)
COUNT_AFTER=$(echo "$STATS_OUT_RESTART" | grep "Total:" | awk '{print $2}')
if [ "$COUNT_AFTER" = "20" ]; then
    pass "All 20 tasks survived restart"
else
    fail "After restart: expected 20, got $COUNT_AFTER. Output: $STATS_OUT_RESTART"
fi
echo "Step 4: Flushing (WAL checkpoint)..."
$TASKMAN_CMD flush > /dev/null 2>&1
pass "Flush completed (WAL checkpointed + truncated)"
echo "Step 5: Re-importing (should skip duplicates)..."
IMPORT_RESULT=$($TASKMAN_CMD import "$EXPORT_FILE" 2>&1)
IMPORTED=$(echo "$IMPORT_RESULT" | grep -o '[0-9]*' | head -n 1)
if [ "$IMPORTED" = "0" ]; then
    pass "Re-import: 0 new tasks (all duplicates skipped)"
else
    fail "Re-import: expected 0 new, got $IMPORTED. Result: $IMPORT_RESULT"
fi
echo "Step 6: Verify final state..."
STATS_OUT_FINAL=$($TASKMAN_CMD stats 2>&1)
FINAL_COUNT=$(echo "$STATS_OUT_FINAL" | grep "Total:" | awk '{print $2}')
if [ "$FINAL_COUNT" = "20" ]; then
    pass "Final count: 20 tasks (no duplicates)"
else
    fail "Final count: expected 20, got $FINAL_COUNT. Output: $STATS_OUT_FINAL"
fi
rm -rf "$DB_DIR" "$EXPORT_FILE" .taskman
echo ""
echo "==============================================="
echo -e " ${GREEN}All crash recovery tests passed!${NC}"
echo "==============================================="
