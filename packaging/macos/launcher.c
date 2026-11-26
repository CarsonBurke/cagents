/*
 * The main executable of the stable Cagents.app bundle.
 *
 * An installation keeps each release in its own tree,
 * PREFIX/versions/VERSION/bin, and PREFIX/current names the one it serves.
 * The bundle at PREFIX/Cagents.app never moves, so the Dock and Login Items
 * keep pointing at it across updates. This launcher replaces
 * itself with the served release's cagents-ui. LaunchServices keeps the
 * launcher's bundle as the identity of the process after the exec, while
 * cagents-ui runs from its own version tree and finds its matched CLI and
 * daemon beside it, as it does on Linux.
 *
 * Everything is resolved to a real path before the exec: a running window
 * stays with the release it started from when `current` moves.
 */
#include <errno.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void report(const char *message)
{
    /* Finder shows nothing a launcher writes to stderr, so say it in a
     * dialog. osascript is part of every macOS installation; the message
     * is an argument, never part of the script. */
    fprintf(stderr, "cagents: %s\n", message);
    execl("/usr/bin/osascript", "osascript",
          "-e", "on run argv",
          "-e", "display alert \"cagents cannot start\" message (item 1 of argv) as critical",
          "-e", "end run",
          message, (char *)NULL);
    _exit(1);
}

/* Strips N trailing components from PATH in place. */
static int strip_components(char *path, int count)
{
    while (count-- > 0) {
        char *slash = strrchr(path, '/');
        if (slash == NULL || slash == path)
            return -1;
        *slash = '\0';
    }
    return 0;
}

int main(int argc, char **argv)
{
    char raw[PATH_MAX];
    uint32_t size = sizeof raw;
    if (_NSGetExecutablePath(raw, &size) != 0)
        report("The launcher's own path is too long.");
    char launcher[PATH_MAX];
    if (realpath(raw, launcher) == NULL)
        report("The launcher cannot find its own location.");
    /* PREFIX/Cagents.app/Contents/MacOS/Cagents */
    if (strip_components(launcher, 4) != 0)
        report("The launcher is not inside an installed Cagents.app.");

    char marker[PATH_MAX];
    char wanted[PATH_MAX];
    char ui[PATH_MAX];
    if (snprintf(marker, sizeof marker, "%s/.cagents-install-root", launcher) >= (int)sizeof marker
        || snprintf(wanted, sizeof wanted, "%s/current/bin/cagents-ui", launcher) >= (int)sizeof wanted)
        report("The installation path is too long.");
    if (access(marker, F_OK) != 0) {
        /* The copy inside a version tree, PREFIX/versions/VERSION/Cagents.app,
         * which LaunchServices may also know: it runs its own version. */
        if (snprintf(marker, sizeof marker, "%s/../../.cagents-install-root", launcher) >= (int)sizeof marker
            || snprintf(wanted, sizeof wanted, "%s/bin/cagents-ui", launcher) >= (int)sizeof wanted)
            report("The installation path is too long.");
        if (access(marker, F_OK) != 0)
            report("This Cagents.app is not part of a cagents installation. Reinstall cagents.");
    }
    if (realpath(wanted, ui) == NULL)
        report("The installed version of cagents is missing. Reinstall cagents.");

    /* LaunchServices may pass a process serial number argument on older
     * systems; cagents-ui does not take one. */
    char **arguments = calloc((size_t)argc + 1, sizeof *arguments);
    if (arguments == NULL)
        report("Out of memory.");
    int count = 0;
    arguments[count++] = ui;
    for (int index = 1; index < argc; index++) {
        if (strncmp(argv[index], "-psn_", 5) != 0)
            arguments[count++] = argv[index];
    }
    arguments[count] = NULL;
    execv(ui, arguments);
    char message[PATH_MAX + 64];
    snprintf(message, sizeof message, "Could not run %s: %s", ui, strerror(errno));
    report(message);
    return 1;
}
