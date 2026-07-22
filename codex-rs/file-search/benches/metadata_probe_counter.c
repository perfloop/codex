#define _GNU_SOURCE
#define _LARGEFILE64_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static _Atomic uint64_t *counter;
static pthread_once_t counter_once = PTHREAD_ONCE_INIT;

static void initialize_counter(void) {
    const char *path = getenv("PERFLOOP_METADATA_COUNTER_FILE");
    if (path == NULL) {
        return;
    }

    int fd = open(path, O_RDWR);
    if (fd < 0) {
        return;
    }

    void *mapping = mmap(
        NULL,
        sizeof(*counter),
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0
    );
    close(fd);
    if (mapping != MAP_FAILED) {
        counter = mapping;
    }
}

static void count_metadata_probe(void) {
    pthread_once(&counter_once, initialize_counter);
    if (counter != NULL) {
        atomic_fetch_add_explicit(counter, 1, memory_order_relaxed);
    }
}

static int (*real_stat)(const char *, struct stat *);
static pthread_once_t stat_once = PTHREAD_ONCE_INIT;

static void resolve_stat(void) {
    real_stat = dlsym(RTLD_NEXT, "stat");
}

int stat(const char *path, struct stat *buffer) {
    pthread_once(&stat_once, resolve_stat);
    if (real_stat == NULL) {
        errno = ENOSYS;
        return -1;
    }
    count_metadata_probe();
    return real_stat(path, buffer);
}

static int (*real_stat64)(const char *, struct stat64 *);
static pthread_once_t stat64_once = PTHREAD_ONCE_INIT;

static void resolve_stat64(void) {
    real_stat64 = dlsym(RTLD_NEXT, "stat64");
}

int stat64(const char *path, struct stat64 *buffer) {
    pthread_once(&stat64_once, resolve_stat64);
    if (real_stat64 == NULL) {
        errno = ENOSYS;
        return -1;
    }
    count_metadata_probe();
    return real_stat64(path, buffer);
}

static int (*real_fstatat)(int, const char *, struct stat *, int);
static pthread_once_t fstatat_once = PTHREAD_ONCE_INIT;

static void resolve_fstatat(void) {
    real_fstatat = dlsym(RTLD_NEXT, "fstatat");
}

int fstatat(int directory_fd, const char *path, struct stat *buffer, int flags) {
    pthread_once(&fstatat_once, resolve_fstatat);
    if (real_fstatat == NULL) {
        errno = ENOSYS;
        return -1;
    }
    count_metadata_probe();
    return real_fstatat(directory_fd, path, buffer, flags);
}
