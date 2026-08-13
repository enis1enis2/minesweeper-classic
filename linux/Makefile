# Makefile for the Linux Minesweeper client (X11 + terminal + headless).
#
# Builds a fully static binary (glibc, pthreads, X11) from the portable core
# in linux/ and the scenario analyzer in ../src/analyze.c.
#
# Targets:
#   make            build the static X11 client (minesweeper)
#   make dynamic    build a dynamically linked client (smaller, needs libX11)
#   make headless   build a static client without X11 (terminal + headless)
#   make clean
#
# HTTP(S) telemetry transport (libcurl):
#   make MS_HTTP=1           add the /ms-sim/ HTTP(S) transport (needs libcurl)
#   make MS_HTTP=1 dynamic   same, dynamically linked (needs libcurl4-openssl-dev)

CC      ?= gcc
CFLAGS  ?= -O2
CFLAGS  += -std=c11 -Wall -Wextra -pthread -I. -I../src \
           -D_DEFAULT_SOURCE -D_POSIX_C_SOURCE=200809L

X11_LIBS := -lX11 -lxcb -lXdmcp -lXau
LDLIBS   := -lpthread -lm

ifdef MS_HTTP
CFLAGS  += -DMS_HTTP
LDLIBS  += -lcurl
endif

SRC      := ms_core.c ms_net.c ms_sha256.c ms_ini.c ms_term.c ms_main.c
X11_SRC  := ms_x11.c
ANALYZE  := ../src/analyze.c

OBJ      := $(SRC:.c=.o)
X11_OBJ  := $(X11_SRC:.c=.o)
ANALYZE_OBJ := analyze.o

all: minesweeper

$(ANALYZE_OBJ): $(ANALYZE)
	$(CC) $(CFLAGS) -c -o $@ $(ANALYZE)

%.o: %.c ms_core.h ms_net.h ms_sha256.h ms_ini.h
	$(CC) $(CFLAGS) -c -o $@ $<

minesweeper: $(OBJ) $(X11_OBJ) $(ANALYZE_OBJ)
	$(CC) -static $(CFLAGS) -o $@ $(OBJ) $(X11_OBJ) $(ANALYZE_OBJ) \
	    $(X11_LIBS) $(LDLIBS)

# dynamic variant (falls back to shared libX11 when static archives are
# unavailable)
dynamic: CFLAGS +=
dynamic: $(OBJ) $(X11_OBJ) $(ANALYZE_OBJ)
	$(CC) $(CFLAGS) -o minesweeper-dyn $(OBJ) $(X11_OBJ) $(ANALYZE_OBJ) \
	    $(X11_LIBS) $(LDLIBS)

# static client without the X11 frontend (terminal + headless only)
headless: $(OBJ) $(ANALYZE_OBJ)
	$(CC) -static $(CFLAGS) -o minesweeper-headless $(OBJ) $(ANALYZE_OBJ) \
	    $(LDLIBS)

# regression test: ini config path must never be shelled out
test: ms_ini.o test/ms_ini_shell.o
	$(CC) $(CFLAGS) -o ms_ini_shell_test ms_ini.o test/ms_ini_shell.o
	./ms_ini_shell_test

clean:
	rm -f $(OBJ) $(X11_OBJ) $(ANALYZE_OBJ) minesweeper minesweeper-dyn \
	    minesweeper-headless ms_ini_shell_test test/ms_ini_shell.o

.PHONY: all dynamic headless test clean
