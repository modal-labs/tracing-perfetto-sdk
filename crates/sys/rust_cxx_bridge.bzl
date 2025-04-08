"""CXX Bridge rules.

Qorix adaptation to the original CXX Bridge rules (https://github.com/dtolnay/cxx/blob/master/tools/bazel/rust_cxx_bridge.bzl)
 for enabling bridge code generation into the same output folder as Cargo build.
"""

load("@bazel_skylib//rules:run_binary.bzl", "run_binary")
load("@rules_cc//cc:defs.bzl", "cc_library")

def rust_cxx_bridge(name, src, outdir, deps = [], linkstatic = True, **kwargs):
    """A macro defining a cxx bridge library

    Args:
        name (string): The name of the new target
        src (string): The rust source file to generate a bridge for
        outdir (string): The output dir for the generated code
        deps (list, optional): A list of dependencies for the underlying cc_library. Defaults to [].
        **kwargs: Common arguments to pass through to underlying rules.
    """
    native.alias(
        name = "%s/header" % name,
        actual = "%s/%s.h" % (outdir, src),
        **kwargs
    )

    native.alias(
        name = "%s/source" % name,
        actual = "%s/%s.cc" % (outdir, src),
        **kwargs
    )

    run_binary(
        name = "%s/generated" % name,
        srcs = [src],
        outs = [
            "%s/%s.h" % (outdir, src),
            "%s/%s.cc" % (outdir, src),
        ],
        args = [
            "$(execpath %s)" % src,
            "-o",
            "$(execpath %s/%s.h)" % (outdir, src),
            "-o",
            "$(execpath %s/%s.cc)" % (outdir, src),
        ],
        tool = "@cxx.rs//:codegen",
        **kwargs
    )

    cc_library(
         name = name,
         srcs = ["%s/%s.cc" % (outdir, src)],
         deps = deps + [":%s/include" % name],
         linkstatic = linkstatic,
         **kwargs
    )

    cc_library(
        name = "%s/include" % name,
        hdrs = ["%s/%s.h" % (outdir, src)],
        **kwargs
    ) 
