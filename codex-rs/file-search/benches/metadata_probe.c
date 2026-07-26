#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <linux/stat.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <sys/stat.h>

static _Atomic unsigned long metadata_calls;

#define WRAP_STAT_CALL(name, arguments, call_arguments)                         \
    static int (*real_##name) arguments;                                        \
    static pthread_once_t name##_once = PTHREAD_ONCE_INIT;                       \
    static void resolve_##name(void) {                                           \
        real_##name = dlsym(RTLD_NEXT, #name);                                   \
    }                                                                             \
    int name arguments {                                                         \
        pthread_once(&name##_once, resolve_##name);                              \
        atomic_fetch_add_explicit(&metadata_calls, 1, memory_order_relaxed);     \
        if (real_##name == NULL) {                                               \
            errno = ENOSYS;                                                      \
            return -1;                                                           \
        }                                                                         \
        return real_##name call_arguments;                                      \
    }

WRAP_STAT_CALL(stat, (const char *path, struct stat *buffer), (path, buffer))
WRAP_STAT_CALL(lstat, (const char *path, struct stat *buffer), (path, buffer))
WRAP_STAT_CALL(
    fstatat,
    (int directory_fd, const char *path, struct stat *buffer, int flags),
    (directory_fd, path, buffer, flags))
WRAP_STAT_CALL(
    newfstatat,
    (int directory_fd, const char *path, struct stat *buffer, int flags),
    (directory_fd, path, buffer, flags))
WRAP_STAT_CALL(
    statx,
    (int directory_fd,
     const char *path,
     int flags,
     unsigned int mask,
     struct statx *buffer),
    (directory_fd, path, flags, mask, buffer))
WRAP_STAT_CALL(
    __xstat,
    (int version, const char *path, struct stat *buffer),
    (version, path, buffer))
WRAP_STAT_CALL(
    __lxstat,
    (int version, const char *path, struct stat *buffer),
    (version, path, buffer))
WRAP_STAT_CALL(
    __fxstatat,
    (int version,
     int directory_fd,
     const char *path,
     struct stat *buffer,
     int flags),
    (version, directory_fd, path, buffer, flags))

__attribute__((destructor)) static void report_metadata_calls(void) {
    fprintf(
        stderr,
        "PERFLOOP_METADATA_CALLS=%lu\n",
        atomic_load_explicit(&metadata_calls, memory_order_relaxed));
}
