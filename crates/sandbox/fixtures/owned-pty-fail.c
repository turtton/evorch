#include <errno.h>
#include <termios.h>
#include <sys/ioctl.h>
int openpty(int *master, int *slave, char *name, const struct termios *term, const struct winsize *size) {
    (void)master; (void)slave; (void)name; (void)term; (void)size;
    errno = EIO; return -1;
}
