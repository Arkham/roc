#!/usr/bin/env bash

# script to return exit code 1 if benchmarks have regressed

# benchmark trunk
ulimit -s unlimited
cd bench-folder-trunk
./target/release/deps/time_bench --bench
cd ..

# move benchmark results so they can be compared later
cp -r bench-folder-trunk/target/criterion bench-folder-branch/target/

cd bench-folder-branch

LOG_FILE="bench_log.txt"
touch $LOG_FILE

FULL_CMD=" ./target/release/deps/time_bench --bench"
echo $FULL_CMD

script -efq $LOG_FILE -c "$FULL_CMD"
EXIT_CODE=$?

if cat $LOG_FILE | grep -q "regressed"; then

    grep -B3 "regressed" $LOG_FILE | sed 's/\x1B\[[0-9;]\{1,\}[A-Za-z]//g' | grep -o "\".*\"" | rev | cut -d' ' -f1 | rev > slow_benches_1.txt
    echo "regression(s) detected in:"
    cat slow_benches_1.txt
    echo ""
    echo ""
    echo "------<<<<<<>>>>>>------"
    echo "Benchmark detected regression. Running benchmark again to confirm..."
    echo "------<<<<<<>>>>>>------"
    echo ""
    echo ""

    # delete criterion folder to remove old benchmark data
    rm -rf ./target/criterion

    # benchmark trunk again
    cd ../bench-folder-trunk
    rm -rf target/criterion
    ./target/release/deps/time_bench --bench

    cd ../bench-folder-branch
    cp -r ../bench-folder-trunk/target/criterion ./target

    rm $LOG_FILE
    touch $LOG_FILE

    script -efq $LOG_FILE -c "$FULL_CMD"
    EXIT_CODE=$?

    if cat $LOG_FILE | grep -q "regressed"; then

        grep -B3 "regressed" $LOG_FILE | sed 's/\x1B\[[0-9;]\{1,\}[A-Za-z]//g' | grep -o "\".*\"" | rev | cut -d' ' -f1 | rev > slow_benches_2.txt
        echo "regression(s) detected in:"
        cat slow_benches_2.txt

        if [[ $(grep -Fxf slow_benches_1.txt slow_benches_2.txt | wc -l) -gt 0 ]]; then
            echo ""
            echo ""
            echo "------<<<<<<!!!!!!>>>>>>------"
            echo "Benchmarks were run twice and a regression was detected both times for the following benchmarks:"
            grep -Fxf slow_benches_1.txt slow_benches_2.txt
            echo "------<<<<<<!!!!!!>>>>>>------"
            echo ""
            echo ""
            exit 1
        else
            echo "Benchmarks were run twice and a regression was detected on one run. We assume this was a fluke."
            exit 0
        fi
    else
        echo "Benchmarks were run twice and a regression was detected on one run. We assume this was a fluke."
        exit 0
    fi
else
    echo ""
    echo "Benchmark execution finished with exit code: $EXIT_CODE."
    echo ""
    exit $EXIT_CODE
fi