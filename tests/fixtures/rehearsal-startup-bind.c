/* Private update-retry fixture: inject only bind(serving socket) EADDRINUSE.
 * ROOT and CANDIDATE are fixed at compilation inside the marked private root.
 * No constructor exits, fake protocol, altered executable or recovery receipt.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <unistd.h>

#ifndef RETRY_ROOT
#error RETRY_ROOT must name a marked private rehearsal
#endif
#ifndef RETRY_CANDIDATE
#error RETRY_CANDIDATE must name the exact selected installed daemon
#endif

int bind(int descriptor, const struct sockaddr *address, socklen_t length) {
    int (*real_bind)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "bind");
    char executable[4096] = {0}, path[sizeof(((struct sockaddr_un *)0)->sun_path) + 1] = {0};
    ssize_t size = readlink("/proc/self/exe", executable, sizeof(executable) - 1);
    if (size < 0) strcpy(executable, "<unavailable>");
    if (address && length >= sizeof(address->sa_family) && address->sa_family == AF_UNIX && length > offsetof(struct sockaddr_un, sun_path)) {
        size_t bytes = length - offsetof(struct sockaddr_un, sun_path);
        if (bytes > sizeof(path) - 1) bytes = sizeof(path) - 1;
        memcpy(path, ((const struct sockaddr_un *)address)->sun_path, bytes);
    }
    const char *decision = "pass-other";
    int fail = 0;
    if (!strcmp(path, RETRY_ROOT "/rt/cagents/cagentsd.sock")) {
        decision = "pass-source";
        if (!strcmp(executable, RETRY_CANDIDATE)) {
            struct stat metadata;
            if (!lstat(RETRY_ROOT "/.fixture/fail-new-daemon.armed", &metadata)) {
                decision = "fail-armed";
                fail = 1;
            } else {
                decision = "pass-disarmed";
            }
        }
    }
    /* Paths can be arbitrary in unrelated processes. Keep each record one line. */
    for (char *p = executable; *p; p++) if (*p == '\n' || *p == '\r' || *p == '\t') *p = '?';
    for (char *p = path; *p; p++) if (*p == '\n' || *p == '\r' || *p == '\t') *p = '?';
    int log = open(RETRY_ROOT "/logs/startup-bind.tsv", O_WRONLY | O_CREAT | O_APPEND | O_NOFOLLOW | O_CLOEXEC, 0600);
    if (log >= 0) {
        char line[4608];
        int count = snprintf(line, sizeof(line), "%ld\t%s\t%s\t%s\n", (long)getpid(), executable, path, decision);
        if (count > 0 && (size_t)count < sizeof(line)) (void)write(log, line, count);
        close(log);
    }
    if (fail) {
        errno = EADDRINUSE;
        return -1;
    }
    return real_bind ? real_bind(descriptor, address, length) : (int)syscall(SYS_bind, descriptor, address, length);
}
