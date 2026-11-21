#define _GNU_SOURCE
#include <assert.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <termios.h>
#include <unistd.h>
static void report(const char *tag) {
    printf("%s %d %d %d %d %d %d %d\n", tag, getpid(), getppid(), getsid(0), getpgrp(), tcgetpgrp(0), isatty(0), isatty(1) && isatty(2));
    fflush(stdout);
}
int main(int argc, char **argv) {
    assert(argc == 2 || argc == 3);
    if (strcmp(argv[1], "io")) {
        int gate[2]; assert(pipe(gate) == 0);
        pid_t pid = fork(); assert(pid >= 0);
        if (!pid) {
            close(gate[0]);
            signal(SIGHUP, SIG_IGN);
            if (!strcmp(argv[1], "escape")) assert(setpgid(0, 0) == 0);
            int control = -1;
            if (argc == 3) {
                control = socket(AF_UNIX, SOCK_STREAM, 0); assert(control >= 0);
                struct sockaddr_un address = { .sun_family = AF_UNIX };
                assert(strlen(argv[2]) < sizeof address.sun_path);
                strcpy(address.sun_path, argv[2]);
                assert(connect(control, (struct sockaddr *)&address, sizeof address) == 0);
            }
            report("DESC");
            assert(write(gate[1], "R", 1) == 1); close(gate[1]);
            if (control >= 0) {
                char probe;
                assert(read(control, &probe, 1) == 1);
                assert(write(control, "A", 1) == 1);
            }
            for (;;) pause();
        }
        close(gate[1]); char b; assert(read(gate[0], &b, 1) == 1); close(gate[0]);
    }
    assert(getenv("ONLY") && !strcmp(getenv("ONLY"), "target"));
    assert(!getenv("SHELL"));
    report("READY");
    char line[64];
    while (fgets(line, sizeof line, stdin)) {
        if (!strncmp(line, "EXIT", 4)) return 37;
        if (!strncmp(line, "SIZE", 4)) {
            struct winsize s; assert(ioctl(0, TIOCGWINSZ, &s) == 0);
            printf("SIZE %d %d\n", s.ws_row, s.ws_col); fflush(stdout);
        }
    }
    puts("EOF"); return 42;
}
