/* Private X11 input-to-window-pixels probe. This is not scanout/frame timing. */
#define _POSIX_C_SOURCE 200809L
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XTest.h>
#include <stdint.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
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
static unsigned char channel(unsigned long pixel, unsigned long mask) {
    if (!mask) return 0;
    while (!(mask & 1)) { mask >>= 1; pixel >>= 1; }
    return (unsigned char)(((pixel & mask) * 255) / mask);
}
static uint64_t pixels(Display *display, Window window, int x, int y, int width, int height, const char *path) {
    XImage *image = XGetImage(display, window, x, y, width, height, AllPlanes, ZPixmap);
    if (!image) { fputs("XGetImage failed\n", stderr); exit(3); }
    FILE *file = path ? fopen(path, "wb") : NULL;
    if (path && !file) { perror("Cannot save calibration pixels"); exit(3); }
    if (file) fprintf(file, "P6\n%d %d\n255\n", width, height);
    uint64_t hash = UINT64_C(1469598103934665603);
    for (int row = 0; row < height; row++) {
        for (int col = 0; col < width; col++) {
            unsigned long pixel = XGetPixel(image, col, row);
            if (file) {
                unsigned char rgb[3] = { channel(pixel, image->red_mask), channel(pixel, image->green_mask), channel(pixel, image->blue_mask) };
                if (fwrite(rgb, 1, sizeof(rgb), file) != sizeof(rgb)) { perror("Cannot write calibration pixels"); exit(3); }
            }
            hash ^= pixel;
            hash *= UINT64_C(1099511628211);
        }
    }
    if (file && fclose(file)) { perror("Cannot close calibration pixels"); exit(3); }
    XDestroyImage(image);
    return hash;
}
int main(int argc, char **argv) {
    if (getenv("CAGENTS_AGENT_AUTHORITY") || argc != 8) return 77;
    Display *display = XOpenDisplay(NULL);
    if (!display) return 2;
    Window window = strtoul(argv[1], NULL, 0);
    int x = atoi(argv[2]), y = atoi(argv[3]), width = atoi(argv[4]), height = atoi(argv[5]);
    int count = atoi(argv[6]);
    if (width < 4 || height < 4 || width > 200 || height > 100 || count < 2 || count > 100) return 2;
    Window child, pointer_root, pointer_child;
    XWindowAttributes attributes;
    int local_x, local_y, screen_x, screen_y, root_x, root_y, pointer_x, pointer_y;
    unsigned int pointer_mask;
    if (!XGetWindowAttributes(display, window, &attributes) || attributes.map_state != IsViewable
        || !XTranslateCoordinates(display, DefaultRootWindow(display), window, x, y, &local_x, &local_y, &child)) return 3;
    x = local_x; y = local_y;
    if (x < 0 || y < 0 || x + width > attributes.width || y + height > attributes.height) return 3;
    if (!XTranslateCoordinates(display, window, DefaultRootWindow(display), x + width / 2, y + height / 2,
                               &screen_x, &screen_y, &child)) return 3;
    Window input_focus;
    int focus_revert;
    XGetInputFocus(display, &input_focus, &focus_revert);
    fprintf(stderr, "{\"event\":\"probe-geometry\",\"window\":%lu,\"roi_window\":[%d,%d,%d,%d],\"hover_root\":[%d,%d],\"depth\":%d,\"input_focus\":%lu}\n",
            window, x, y, width, height, screen_x, screen_y, attributes.depth, input_focus);
    size_t path_size = strlen(argv[7]) + 32;
    char *path = malloc(path_size);
    if (!path) return 3;
    uint64_t reference[2];
    for (int active = 0; active < 2; active++) {
        double started = now();
        if (!XTestFakeMotionEvent(display, DefaultScreen(display), active ? screen_x : 5,
                                 active ? screen_y : 400, CurrentTime)) return 3;
        XSync(display, False);
        if (!XQueryPointer(display, window, &pointer_root, &pointer_child, &root_x, &root_y,
                           &pointer_x, &pointer_y, &pointer_mask)) return 3;
        fprintf(stderr, "{\"event\":\"calibration-pointer\",\"hover\":%s,\"root\":[%d,%d],\"window\":[%d,%d]}\n",
                active ? "true" : "false", root_x, root_y, pointer_x, pointer_y);
        if (root_x != (active ? screen_x : 5) || root_y != (active ? screen_y : 400)) {
            puts("{\"calibrated\":false,\"reason\":\"pointer did not reach the requested position\"}");
            free(path); XCloseDisplay(display); return 4;
        }
        uint64_t previous = pixels(display, window, x, y, width, height, NULL);
        double stable_since = now();
        int settled = 0;
        while (now() - started < 1.0) {
            pause_ms(8);
            uint64_t current = pixels(display, window, x, y, width, height, NULL);
            double observed = now();
            if (current != previous) { previous = current; stable_since = observed; }
            if (observed - started < 1.0 && (!active || current != reference[0]) && observed - stable_since >= .15) { settled = 1; break; }
        }
        snprintf(path, path_size, "%s-calibration-%d.ppm", argv[7], active);
        reference[active] = pixels(display, window, x, y, width, height, path);
        fprintf(stderr, "{\"event\":\"calibration-reference\",\"hover\":%s,\"settled\":%s,\"seconds\":%.9f,\"hash\":\"%" PRIu64 "\"}\n",
                active ? "true" : "false", settled ? "true" : "false", now() - started, reference[active]);
        if (!settled || now() - started >= 1.0 || reference[active] != previous) {
            puts("{\"calibrated\":false,\"reason\":\"distinct control reference did not settle within one second\"}");
            free(path); XCloseDisplay(display); return 4;
        }
    }
    free(path);
    if (reference[0] == reference[1]) {
        puts("{\"calibrated\":false,\"reason\":\"hover did not alter the observed control\"}");
        XCloseDisplay(display);
        return 4;
    }
    puts("{\"calibrated\":true,\"metric\":\"input-to-observed-window-pixels\",\"poll_ms\":8}");
    for (int index = 0; index < count; index++) {
        int active = index % 2;
        double start = now(), last = start, max_gap = 0;
        if (!XTestFakeMotionEvent(display, DefaultScreen(display), active ? screen_x : 5,
                                 active ? screen_y : 400, CurrentTime)) return 3;
        XFlush(display);
        int reached = 0;
        double elapsed;
        do {
            double sampled = now(), gap = sampled - last;
            if (gap > max_gap) max_gap = gap;
            last = sampled;
            reached = pixels(display, window, x, y, width, height, NULL) == reference[active];
            elapsed = now() - start;
            double read_duration = now() - sampled;
            if (read_duration > max_gap) max_gap = read_duration;
            if (reached || elapsed >= 1.0) break;
            pause_ms(8);
        } while (1);
        printf("{\"sample\":%d,\"hover\":%s,\"started_monotonic\":%.9f,\"seconds\":%.9f,\"max_poll_gap_seconds\":%.9f,\"timeout\":%s}\n",
               index, active ? "true" : "false", start, elapsed, max_gap, reached && elapsed < 1.0 && max_gap < 1.0 ? "false" : "true");
        fflush(stdout);
        pause_ms(120);
    }
    XCloseDisplay(display);
    return 0;
}
