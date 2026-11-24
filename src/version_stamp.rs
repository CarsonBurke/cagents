//! Both compilation roots, the library and the native hot root, declare this
//! module rather than include it with `modules.rs`: a crate can name a
//! `macro_export` macro by path only when no macro expansion defined it.

/// Records the version of the executable that expands it. Invoke it once,
/// as an item at the root of each executable.
///
/// It stamps from a constructor that runs before `main` and before the
/// unprioritized `.init_array` constructors, such as the holders' that serve
/// in place of `main`, so nothing in the process reads
/// [`version`](crate::version) first. Standard library initialization uses
/// priority 99; this runs just after it.
///
/// Mach-O initializers have no priorities. dyld runs an image's initializers
/// in link order, where the executable's own objects precede the libraries it
/// links, so this one, expanded in the executable, still runs first there; a
/// read that ever came first would make the stamp panic rather than misreport.
/// Darwin's standard library needs no initializer to read its arguments.
///
/// The metadata is read where this expands, so only the executable's own
/// compilation depends on it: a new commit relinks the executables and leaves
/// the library compiled. See `build.rs` for where the metadata comes from.
#[macro_export]
macro_rules! stamp_version {
    () => {
        const _: () = {
            extern "C" fn stamp_version() {
                $crate::build_version::stamp(
                    option_env!("CAGENTS_BUILD_METADATA"),
                    option_env!("CAGENTS_CHECKOUT_METADATA"),
                )
            }
            #[used]
            #[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array.00100"))]
            #[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
            static STAMP_VERSION: extern "C" fn() = stamp_version;
        };
    };
}
