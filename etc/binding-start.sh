#!/bin/bash

cd $(dirname $0)/..
CONFDIR=`pwd`/etc
ROOTDIR=`pwd`/..

# use libafb development version if any
export LD_LIBRARY_PATH="/usr/local/lib64:$LD_LIBRARY_PATH"
export PATH="/usr/local/lib64:$PATH"
clear
pwd

export SIMULATION_MODE=injector
export SIMULATION_PORT=1234
export SIMULATION_DEBUG=""

for PRM in $*; do
    case $PRM in
        --mode=inj*)
            SIMULATION_MODE=injector
            ;;

        --mode=resp*)
            SIMULATION_MODE=responder
            ;;

        --port=*)
            SIMULATION_PORT=$(echo $PRM | awk -F = '{print $2}')
            ;;

        --debug=*)
            SIMULATION_DEBUG=$(echo $PRM | awk -F = '{print 2}')
            ;;

        *)
            echo "Syntax $0 --mode=simulator|injector [--port=1234] [--debug='-vvv -traceapi=common']"
            exit 1
            ;;
    esac
done
echo "config: MODE=$SIMULATION_MODE http_port=$SIMULATION_PORT"

for BINDING in libafb_injector.so
do
    if ! test -f $CARGO_TARGET_DIR/debug/$BINDING; then
        echo "FATAL: missing $CARGO_TARGET_DIR/debug/$BINDING use: cargo build"
        exit 1
    fi
done

# start binder with test config
afb-binder -v \
   --config=$CONFDIR/binder-injector.yaml \
   --config=$CONFDIR/binding-injector.yaml \
   --port=$SIMULATION_PORT $SIMULATION_DEBUG
