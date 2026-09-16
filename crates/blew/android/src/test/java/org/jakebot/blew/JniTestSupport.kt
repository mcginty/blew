package org.jakebot.blew

import java.io.File
import java.lang.reflect.Method

/** Shared by the tests that check each direction of the Rust/Kotlin JNI boundary. */
internal object JniTestSupport {
    class RustSource(
        val path: String,
        val text: String,
    )

    /** Every Rust source file in the workspace, keyed by its repo-relative path. */
    fun rustSources(): List<RustSource> {
        val root = repoRoot()
        return File(root, "crates")
            .listFiles { f -> File(f, "src").isDirectory }
            .orEmpty()
            .flatMap { crate -> File(crate, "src").walkTopDown().filter { it.isFile && it.extension == "rs" } }
            .sortedBy { it.path }
            .map { RustSource(it.relativeTo(root).path, it.readText()) }
    }

    fun descriptorOf(method: Method): String =
        method.parameterTypes.joinToString("", prefix = "(", postfix = ")") { descriptorOf(it) } +
            descriptorOf(method.returnType)

    fun descriptorOf(type: Class<*>): String =
        when {
            type.isArray -> "[" + descriptorOf(checkNotNull(type.componentType))
            !type.isPrimitive -> "L" + type.name.replace('.', '/') + ";"
            else -> PRIMITIVE_DESCRIPTORS.getValue(type.name)
        }

    private fun repoRoot(): File {
        val start = File(System.getProperty("user.dir") ?: ".").absoluteFile
        return generateSequence(start) { it.parentFile }
            .firstOrNull { File(it, ROOT_MARKER).isDirectory }
            ?: throw AssertionError("could not find $ROOT_MARKER above $start")
    }

    private const val ROOT_MARKER = "crates/blew/src/platform/android"

    private val PRIMITIVE_DESCRIPTORS =
        mapOf(
            "void" to "V",
            "boolean" to "Z",
            "byte" to "B",
            "char" to "C",
            "short" to "S",
            "int" to "I",
            "long" to "J",
            "float" to "F",
            "double" to "D",
        )
}
