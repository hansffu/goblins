/* Test-only transport fixture: the real CLI keeps its fixed socket path.
 * Connect can only adopt the already-connected fake peer inherited from
 * the test. It never opens or falls back to a real daemon connection. */
#include <errno.h>
#include <limits.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/un.h>
#include <unistd.h>

/* Installed in the child before exec. Without the hook, the real CLI's
 * connect fails before it can initialize or mutate any daemon. */
int block_connect_syscall(void) {
#if defined(__x86_64__)
    struct sock_filter instructions[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
        /* x32 shares the audit architecture; deny its connect number too. */
        BPF_STMT(BPF_ALU | BPF_AND | BPF_K, 0x3fffffff),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_connect, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EACCES),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };
    struct sock_fprog program = {
        .len = sizeof(instructions) / sizeof(instructions[0]),
        .filter = instructions,
    };
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL)) return -1;
    return prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program);
#else
    errno = ENOTSUP;
    return -1;
#endif
}

int connect(int fd, const struct sockaddr *address, socklen_t length) {
    const char expected[] = "/run/goblins/request.sock";
    const char *raw = getenv("GOBLINS_TEST_FAKE_REQUEST_FD");
    size_t offset = offsetof(struct sockaddr_un, sun_path);
    if (!address || address->sa_family != AF_UNIX || length <= offset || !raw || !*raw) {
        errno = EACCES;
        return -1;
    }
    const struct sockaddr_un *requested = (const struct sockaddr_un *)address;
    size_t available = length - offset;
    if (available > sizeof(requested->sun_path)) available = sizeof(requested->sun_path);
    if (strnlen(requested->sun_path, available) != sizeof(expected) - 1 ||
        memcmp(requested->sun_path, expected, sizeof(expected) - 1)) {
        errno = EACCES;
        return -1;
    }
    char *end;
    long inherited = strtol(raw, &end, 10);
    if (*end || inherited < 0 || inherited > INT_MAX) {
        errno = EACCES;
        return -1;
    }
    return dup2((int)inherited, fd) < 0 ? -1 : 0;
}
