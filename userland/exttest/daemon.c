/* `ext_daemon`: the program of the test extension's services, as an
 * extension's are: linked against libc.so and its image's own libexttest.so.
 *
 * `exit` prints a line and exits. `serve <caps_drop>` says it is ready on its
 * notifier, then answers one line per connection on every socket init handed
 * it: `who`, `fds`, `say <text>`, `spew <bytes>` and `crash`. `caps_drop` is
 * the number of the call that answers the capabilities it holds. */

#include <errno.h>
#include <poll.h>
#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
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

static void answer(int fd) {
    char line[512];
    size_t have = 0;
    while (have + 1 < sizeof line) {
        ssize_t n = read(fd, line + have, 1);
        if (n <= 0 || line[have] == '\n') {
            break;
        }
        have += (size_t)n;
    }
    line[have] = '\0';
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

int main(int argc, char **argv) {
    started_ms = now_ms();
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
