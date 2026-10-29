/* Private X11 input-to-window-pixels probe. This is not scanout/frame timing. */
#define _POSIX_C_SOURCE 200809L
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XTest.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static double now(void) {
    struct timespec value;
    clock_gettime(CLOCK_MONOTONIC, &value);
    return value.tv_sec + value.tv_nsec / 1e9;
}
static void pause_ms(long ms) {
    struct timespec value = { ms / 1000, (ms % 1000) * 1000000 };
    nanosleep(&value, NULL);
}
static uint64_t pixels(Display *display, Window window, int x, int y, int width, int height) {
    XImage *image = XGetImage(display, window, x, y, width, height, AllPlanes, ZPixmap);
    if (!image) { fputs("XGetImage failed\n", stderr); exit(3); }
    uint64_t hash = UINT64_C(1469598103934665603);
    for (int row = 0; row < height; row++) {
        for (int col = 0; col < width; col++) {
            hash ^= XGetPixel(image, col, row);
            hash *= UINT64_C(1099511628211);
        }
    }
    XDestroyImage(image);
    return hash;
}
int main(int argc, char **argv) {
    if (getenv("CAGENTS_AGENT_AUTHORITY") || argc != 7) return 77;
    Display *display = XOpenDisplay(NULL);
    if (!display) return 2;
    Window window = strtoul(argv[1], NULL, 0);
    int x = atoi(argv[2]), y = atoi(argv[3]), width = atoi(argv[4]), height = atoi(argv[5]);
    int count = atoi(argv[6]);
    if (width < 4 || height < 4 || width > 200 || height > 100 || count < 2 || count > 100) return 2;
    Window child;
    int screen_x, screen_y;
    XTranslateCoordinates(display, window, DefaultRootWindow(display), x + width / 2, y + height / 2,
                          &screen_x, &screen_y, &child);
    uint64_t reference[2];
    for (int active = 0; active < 2; active++) {
        XTestFakeMotionEvent(display, DefaultScreen(display), active ? screen_x : 5,
                             active ? screen_y : 400, CurrentTime);
        XSync(display, False);
        pause_ms(180);
        reference[active] = pixels(display, window, x, y, width, height);
    }
    if (reference[0] == reference[1]) {
        puts("{\"calibrated\":false,\"reason\":\"hover did not alter the observed control\"}");
        XCloseDisplay(display);
        return 4;
    }
    puts("{\"calibrated\":true,\"metric\":\"input-to-observed-window-pixels\",\"poll_ms\":8}");
    for (int index = 0; index < count; index++) {
        int active = index % 2;
        double start = now(), last = start, max_gap = 0;
        XTestFakeMotionEvent(display, DefaultScreen(display), active ? screen_x : 5,
                             active ? screen_y : 400, CurrentTime);
        XFlush(display);
        int reached = 0;
        double elapsed;
        do {
            double sampled = now(), gap = sampled - last;
            if (gap > max_gap) max_gap = gap;
            last = sampled;
            reached = pixels(display, window, x, y, width, height) == reference[active];
            elapsed = now() - start;
            double read_duration = now() - sampled;
            if (read_duration > max_gap) max_gap = read_duration;
            if (reached || elapsed >= 1.0) break;
            pause_ms(8);
        } while (1);
        printf("{\"sample\":%d,\"hover\":%s,\"started_monotonic\":%.9f,\"seconds\":%.9f,\"max_poll_gap_seconds\":%.9f,\"timeout\":%s}\n",
               index, active ? "true" : "false", start, elapsed, max_gap, reached ? "false" : "true");
        fflush(stdout);
        pause_ms(120);
    }
    XCloseDisplay(display);
    return 0;
}
