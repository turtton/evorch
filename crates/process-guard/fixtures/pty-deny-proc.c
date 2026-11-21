#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <spawn.h>
#include <string.h>
#include <unistd.h>

static int denied(const char *path) {
    return !strncmp(path, "/proc/", 6) && strstr(path, "/fd/");
}

int execve(const char *path, char *const argv[], char *const env[]) {
    if (denied(path)) { errno = EACCES; return -1; }
    int (*real)(const char *, char *const[], char *const[]) = dlsym(RTLD_NEXT, "execve");
    return real(path, argv, env);
}

#define SPAWN(name) \
int name(pid_t *pid, const char *path, const posix_spawn_file_actions_t *actions, \
         const posix_spawnattr_t *attributes, char *const argv[], char *const env[]) { \
    if (denied(path)) return EACCES; \
    int (*real)(pid_t *, const char *, const posix_spawn_file_actions_t *, \
        const posix_spawnattr_t *, char *const[], char *const[]) = dlsym(RTLD_NEXT, #name); \
    return real(pid, path, actions, attributes, argv, env); \
}
SPAWN(posix_spawn)
SPAWN(posix_spawnp)
