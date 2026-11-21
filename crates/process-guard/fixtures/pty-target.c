#define _GNU_SOURCE
#include <assert.h>
#include <dirent.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <termios.h>
#include <unistd.h>

static void ready(const char *tag) {
    printf("%s %d %d %d %d %d %d %d\n", tag, getpid(), getsid(0),
           getpgrp(), tcgetpgrp(0), isatty(0), isatty(1), isatty(2));
    fflush(stdout);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    // A native exec target, not Rust startup (which can normalize signals/fds).
    DIR *fds = opendir("/proc/self/fd");
    assert(fds);
    struct dirent *entry;
    int extras = 0;
    while ((entry = readdir(fds))) {
        int fd = atoi(entry->d_name);
        if (fd > 2 && fd != dirfd(fds)) ++extras;
    }
    closedir(fds);
    printf("FDS %d\n", extras);
    sigset_t mask;
    assert(sigprocmask(SIG_SETMASK, NULL, &mask) == 0);
    for (int sig = 1; sig <= 64; ++sig) {
        if (sig == SIGKILL || sig == SIGSTOP || sig == 32 || sig == 33) continue;
        struct sigaction action;
        assert(sigaction(sig, NULL, &action) == 0);
        assert(action.sa_handler == SIG_DFL);
        assert(!sigismember(&mask, sig));
    }
    if (!strcmp(argv[1], "group")) {
        int sync[2];
        assert(pipe(sync) == 0);
        pid_t child = fork();
        assert(child >= 0);
        if (!child) {
            close(sync[0]);
            ready("DESC");
            assert(write(sync[1], "R", 1) == 1);
            close(sync[1]);
            for (;;) pause();
        }
        close(sync[1]);
        char byte;
        assert(read(sync[0], &byte, 1) == 1);
        close(sync[0]);
    }
    ready("READY");
    char line[64];
    while (fgets(line, sizeof line, stdin)) {
        if (!strncmp(line, "EXIT", 4)) return 37;
        if (!strncmp(line, "SIZE", 4)) {
            struct winsize size;
            assert(ioctl(0, TIOCGWINSZ, &size) == 0);
            printf("SIZE %d %d\n", size.ws_row, size.ws_col);
            fflush(stdout);
        }
    }
    puts("EOF");
    return 42;
}
