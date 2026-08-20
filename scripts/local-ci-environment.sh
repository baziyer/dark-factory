#!/bin/sh

# This file is sourced once at the local-ci entry boundary. Keep the list
# narrow: these values identify a live Dark Factory runtime or task and can
# redirect source tests away from their own temporary fixtures.
unset DARK_FACTORY_HOME \
    DARK_FACTORY_SOCKET \
    DARK_FACTORY_PROJECT \
    DARK_FACTORY_AGENT \
    DARK_FACTORY_SESSION \
    DARK_FACTORY_SESSION_TOKEN_FILE \
    DARK_FACTORY_AGENT_DIR \
    DARK_FACTORY_FACTORYCTL \
    DARK_FACTORY_TASK \
    DARK_FACTORY_RUN
