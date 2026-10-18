/* `ext_daemon`: the program of the test extension's services, mechanisms
 * and clients, as an extension's are: linked against libc.so and its image's
 * own libexttest.so.
 *
 * `exit` prints a line and exits. `serve <caps_drop>` says it is ready on its
 * notifier, then answers one line per connection on every socket init handed
 * it: `who`, `fds`, `read <path>`, `say <text>`, `spew <bytes>` and `crash`.
 * `who <caps_drop>` prints who it runs as and exits. `client <caps_drop>`
 * connects to privd once and answers each line on fd 0: `ask <action>` asks
 * for it with `who`'s arguments, `map <path>` maps that file executable.
 * `caps_drop` is the number of the call that answers the capabilities it
 * holds. */

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define NOTIFIER_FD 3
#define LISTEN_FDS_START 4
#define LISTEN_MAX 8

const char *exttest_greeting(void);

static long caps_drop_nr;
static long long started_ms;
static long long ready_ms;

static long long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void reply(int fd, const char *text) {
    size_t left = strlen(text);
    while (left > 0) {
        ssize_t n = write(fd, text, left);
        if (n <= 0) {
            return;
        }
        text += n;
        left -= (size_t)n;
    }
}

/* A line from `fd` without its newline; -1 at end of file. */
static int read_line(int fd, char *line, size_t size) {
    size_t have = 0;
    ssize_t n = 0;
    while (have + 1 < size) {
        n = read(fd, line + have, 1);
        if (n <= 0 || line[have] == '\n') {
            break;
        }
        have += (size_t)n;
    }
    line[have] = '\0';
    return n <= 0 && have == 0 ? -1 : 0;
}

static void answer(int fd) {
    char line[512];
    read_line(fd, line, sizeof line);
    char out[1024];
    if (strcmp(line, "who") == 0) {
        struct passwd *me = getpwuid(getuid());
        long caps = syscall(caps_drop_nr, 0L);
        snprintf(out, sizeof out,
                 "pid=%d uid=%d gid=%d caps=%ld user=%s started=%lld ready=%lld lib=%s\n",
                 (int)getpid(), (int)getuid(), (int)getgid(), caps,
                 me ? me->pw_name : "-", started_ms, ready_ms, exttest_greeting());
        reply(fd, out);
    } else if (strcmp(line, "fds") == 0) {
        const char *count = getenv("LISTEN_FDS");
        const char *names = getenv("LISTEN_FDNAMES");
        char cwd[256];
        snprintf(out, sizeof out, "count=%s names=%s cwd=%s home=%s\n", count ? count : "-",
                 names ? names : "-", getcwd(cwd, sizeof cwd) ? cwd : "-",
                 getenv("HOME") ? getenv("HOME") : "-");
        reply(fd, out);
    } else if (strncmp(line, "read ", 5) == 0) {
        FILE *file = fopen(line + 5, "r");
        char first[256] = "-\n";
        if (!file || !fgets(first, sizeof first, file)) {
            snprintf(first, sizeof first, "-\n");
        }
        if (file) {
            fclose(file);
        }
        reply(fd, first);
    } else if (strncmp(line, "say ", 4) == 0) {
        printf("%s\n", line + 4);
        fflush(stdout);
        reply(fd, "ok\n");
    } else if (strncmp(line, "spew ", 5) == 0) {
        long bytes = atol(line + 5);
        static const char row[] = "spew spew spew spew spew spew spew spew spew spew spew spew\n";
        for (long done = 0; done < bytes; done += (long)sizeof row - 1) {
            fputs(row, stdout);
        }
        fflush(stdout);
        reply(fd, "ok\n");
    } else if (strcmp(line, "crash") == 0) {
        reply(fd, "bye\n");
        exit(3);
    } else {
        reply(fd, "?\n");
    }
}

static int who(void) {
    char cwd[256];
    const char *path = getenv("PATH");
    const char *action = getenv("PRIV_ACTION");
    printf("uid=%d gid=%d caps=%ld cwd=%s path=%s action=%s lib=%s\n", (int)getuid(),
           (int)getgid(), (long)syscall(caps_drop_nr, 0L), getcwd(cwd, sizeof cwd) ? cwd : "-",
           path ? path : "-", action ? action : "-", exttest_greeting());
    return 0;
}

static void put_be32(unsigned char *at, size_t value) {
    at[0] = (unsigned char)(value >> 24);
    at[1] = (unsigned char)(value >> 16);
    at[2] = (unsigned char)(value >> 8);
    at[3] = (unsigned char)value;
}

static size_t be32(const unsigned char *at) {
    return (size_t)at[0] << 24 | (size_t)at[1] << 16 | (size_t)at[2] << 8 | (size_t)at[3];
}

static size_t put_field(unsigned char *out, const char *key, const char *value) {
    size_t klen = strlen(key);
    size_t vlen = strlen(value);
    out[0] = (unsigned char)klen;
    memcpy(out + 1, key, klen);
    put_be32(out + 1 + klen, vlen);
    memcpy(out + 5 + klen, value, vlen);
    return 5 + klen + vlen;
}

/* A request for `action` with `who`'s arguments, its stdio /dev/null. */
static int send_request(int privd, const char *action, const char *caps) {
    unsigned char frame[512];
    size_t len = 5;
    len += put_field(frame + len, "id", "1");
    len += put_field(frame + len, "action", action);
    len += put_field(frame + len, "arg", "who");
    len += put_field(frame + len, "arg", caps);
    frame[0] = 0x01;
    put_be32(frame + 1, len - 5);
    int null = open("/dev/null", O_RDWR);
    if (null < 0) {
        return -1;
    }
    int fds[3] = {null, null, null};
    struct iovec iov = {frame, len};
    union {
        struct cmsghdr align;
        char buf[CMSG_SPACE(sizeof fds)];
    } control;
    memset(&control, 0, sizeof control);
    struct msghdr msg;
    memset(&msg, 0, sizeof msg);
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.buf;
    msg.msg_controllen = sizeof control.buf;
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof fds);
    memcpy(CMSG_DATA(cmsg), fds, sizeof fds);
    ssize_t sent = sendmsg(privd, &msg, 0);
    close(null);
    return sent == (ssize_t)len ? 0 : -1;
}

static int read_full(int fd, unsigned char *buf, size_t len) {
    while (len > 0) {
        ssize_t n = read(fd, buf, len);
        if (n <= 0) {
            return -1;
        }
        buf += n;
        len -= (size_t)n;
    }
    return 0;
}

/* The value of `key` in a reply's fields, or "-". */
static void field(const unsigned char *at, size_t len, const char *key, char *out, size_t size) {
    snprintf(out, size, "-");
    while (len >= 5) {
        size_t klen = at[0];
        if (len < 5 + klen) {
            return;
        }
        size_t vlen = be32(at + 1 + klen);
        if (len < 5 + klen + vlen) {
            return;
        }
        if (klen == strlen(key) && memcmp(at + 1, key, klen) == 0) {
            snprintf(out, size, "%.*s", (int)vlen, (const char *)(at + 5 + klen));
            return;
        }
        at += 5 + klen + vlen;
        len -= 5 + klen + vlen;
    }
}

/* How the request just sent went: `EXIT <code>`, `ASKED` or `DENIED <why>`. */
static void reply_to(int privd, char *out, size_t size) {
    static unsigned char payload[4096];
    for (;;) {
        unsigned char head[5];
        if (read_full(privd, head, sizeof head) != 0) {
            snprintf(out, size, "LOST\n");
            return;
        }
        size_t len = be32(head + 1);
        if (len > sizeof payload || read_full(privd, payload, len) != 0) {
            snprintf(out, size, "LOST\n");
            return;
        }
        char value[600];
        switch (head[0]) {
        case 0x11:
            field(payload, len, "code", value, sizeof value);
            snprintf(out, size, "EXIT %s\n", value);
            return;
        case 0x12:
            field(payload, len, "reason", value, sizeof value);
            snprintf(out, size, "DENIED %s\n", value);
            return;
        case 0x13:
            snprintf(out, size, "ASKED\n");
            return;
        default:
            break;
        }
    }
}

/* One connection to privd, then a line on fd 1 for each line on fd 0:
 * `ask <action>` asks for it, `map <path>` maps that file executable. */
static int client(const char *caps) {
    int privd = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof addr);
    addr.sun_family = AF_UNIX;
    strcpy(addr.sun_path, "/run/privd");
    if (privd < 0 || connect(privd, (struct sockaddr *)&addr, sizeof addr) != 0) {
        reply(1, "LOST\n");
        return 1;
    }
    char line[512];
    char out[700];
    while (read_line(0, line, sizeof line) == 0) {
        if (strncmp(line, "ask ", 4) == 0) {
            if (send_request(privd, line + 4, caps) != 0) {
                snprintf(out, sizeof out, "LOST\n");
            } else {
                reply_to(privd, out, sizeof out);
            }
        } else if (strncmp(line, "map ", 4) == 0) {
            int file = open(line + 4, O_RDONLY);
            void *at = file < 0 ? MAP_FAILED : mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, file, 0);
            snprintf(out, sizeof out, at == MAP_FAILED ? "failed\n" : "mapped\n");
        } else {
            snprintf(out, sizeof out, "?\n");
        }
        reply(1, out);
    }
    return 0;
}

int main(int argc, char **argv) {
    started_ms = now_ms();
    if (argc >= 3 && strcmp(argv[1], "client") == 0) {
        return client(argv[2]);
    }
    if (argc >= 3 && strcmp(argv[1], "who") == 0) {
        caps_drop_nr = atol(argv[2]);
        return who();
    }
    printf("ext_daemon: pid %d uid %d %s\n", (int)getpid(), (int)getuid(), exttest_greeting());
    fflush(stdout);
    if (argc < 2 || strcmp(argv[1], "exit") == 0) {
        return 0;
    }
    if (strcmp(argv[1], "serve") != 0 || argc < 3) {
        return 2;
    }
    caps_drop_nr = atol(argv[2]);
    const char *count = getenv("LISTEN_FDS");
    int sockets = count ? atoi(count) : 0;
    if (sockets > LISTEN_MAX) {
        sockets = LISTEN_MAX;
    }
    ready_ms = now_ms();
    reply(NOTIFIER_FD, "\n");
    close(NOTIFIER_FD);
    struct pollfd fds[LISTEN_MAX];
    for (int i = 0; i < sockets; i++) {
        fds[i].fd = LISTEN_FDS_START + i;
        fds[i].events = POLLIN;
    }
    for (;;) {
        if (sockets == 0) {
            sleep(60);
            continue;
        }
        if (poll(fds, (nfds_t)sockets, -1) < 0) {
            if (errno == EINTR) {
                continue;
            }
            return 1;
        }
        for (int i = 0; i < sockets; i++) {
            if (!(fds[i].revents & POLLIN)) {
                continue;
            }
            int conn = accept(fds[i].fd, NULL, NULL);
            if (conn >= 0) {
                answer(conn);
                close(conn);
            }
        }
    }
}
