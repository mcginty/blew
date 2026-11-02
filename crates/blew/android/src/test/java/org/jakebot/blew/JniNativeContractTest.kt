package org.jakebot.blew

import org.jakebot.blew.JniTestSupport.descriptorOf
import org.jakebot.blew.JniTestSupport.rustSources
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.File
import java.lang.reflect.Method
import java.lang.reflect.Modifier
import java.net.JarURLConnection
import java.util.jar.JarFile

/**
 * Asserts that every Kotlin `external fun` has a Rust `Java_*` export with the
 * same symbol and the same argument types, and that every export has a Kotlin
 * declaration to bind it. [JniContractTest] checks the other direction.
 *
 * The JVM binds a native method lazily, by symbol name alone, the first time
 * it is called. A name that doesn't line up is an `UnsatisfiedLinkError` only
 * when that callback first fires, which for an error path can be a long way
 * from CI. Types that don't line up raise nothing at all: the export reads
 * arguments the JVM never passed, which is undefined behaviour.
 *
 * Both sides are derived: the Kotlin by reflecting over every class in the
 * package, the Rust by parsing every source file in the workspace.
 */
class JniNativeContractTest {
    private data class Native(
        val descriptor: String,
        val origin: String,
    )

    @Test
    fun `every Java_ export is parsed`() {
        val sources = rustSources()
        val declared = sources.sumOf { EXPORT_MARKER.findAll(it.text).count() }
        assertTrue("found no Java_ exports to check", declared > 0)
        assertEquals(
            "the Java_ exports and the parser have diverged; " +
                "update EXPORT in this test to match the new declaration shape",
            declared,
            sources.sumOf { EXPORT.findAll(it.text).count() },
        )
    }

    @Test
    fun `every bind_java_type binding is parsed`() {
        val sources = rustSources()
        assertEquals(
            "a bind_java_type! use is not in the `pub Name => java.Class` shorthand this test parses; " +
                "update BINDING in this test to match",
            sources.sumOf { BINDING_MARKER.findAll(it.text).count() },
            sources.sumOf { BINDING.findAll(it.text).count() },
        )
    }

    @Test
    fun `Kotlin natives and Rust exports agree on symbol and descriptor`() {
        val failures = mutableListOf<String>()
        val kotlin = kotlinNatives(failures)
        val rust = rustExports(failures)
        assertTrue("found no Kotlin external funs to check", kotlin.isNotEmpty())

        for ((symbol, declared) in kotlin) {
            val export = rust[symbol]
            when {
                export == null -> {
                    failures += "${declared.origin}: no Rust export $symbol -- " +
                        "UnsatisfiedLinkError on first call"
                }

                export.descriptor != declared.descriptor -> {
                    failures += "${declared.origin}: Kotlin declares ${declared.descriptor} " +
                        "but ${export.origin} implements ${export.descriptor}"
                }
            }
        }
        for ((symbol, export) in rust) {
            if (symbol !in kotlin) {
                failures += "${export.origin}: $symbol has no Kotlin external fun binding it"
            }
        }

        if (failures.isNotEmpty()) {
            fail(failures.joinToString("\n", prefix = "JNI native contract violations:\n"))
        }
    }

    private fun kotlinNatives(failures: MutableList<String>): Map<String, Native> {
        val natives = mutableMapOf<String, Native>()
        for (cls in packageClasses()) {
            val methods = cls.declaredMethods.filter { Modifier.isNative(it.modifiers) }
            for (method in methods) {
                val origin = "${cls.name}.${method.name}"
                if (!Modifier.isStatic(method.modifiers)) {
                    // The Rust exports all take `JClass`, which is only what the
                    // JVM passes to a static native.
                    failures += "$origin: external fun is not static -- add @JvmStatic"
                }
                val overloaded = methods.count { it.name == method.name } > 1
                natives[jniSymbol(cls, method, overloaded)] = Native(descriptorOf(method), origin)
            }
        }
        return natives
    }

    /**
     * Rust types the workspace binds to one Java class with `bind_java_type!`,
     * by their unqualified name. A name bound twice, or shadowing a built-in
     * entry of [RUST_DESCRIPTORS], is a failure: it would no longer have one
     * JVM meaning.
     */
    private fun boundJavaTypes(failures: MutableList<String>): Map<String, String> {
        val bound = mutableMapOf<String, String>()
        for (source in rustSources()) {
            for (match in BINDING.findAll(source.text)) {
                val (name, javaClass) = match.destructured
                val descriptor = "L" + javaClass.replace('.', '/') + ";"
                val previous = bound.put(name, descriptor)
                if (name in RUST_DESCRIPTORS || (previous != null && previous != descriptor)) {
                    failures += "${source.path}: bind_java_type! `$name` is ambiguous; give it a distinct name"
                }
            }
        }
        return bound
    }

    private fun rustExports(failures: MutableList<String>): Map<String, Native> {
        val exports = mutableMapOf<String, Native>()
        val descriptors = RUST_DESCRIPTORS + boundJavaTypes(failures)
        for (source in rustSources()) {
            for (match in EXPORT.findAll(source.text)) {
                val (attributes, symbol, params, returnType) = match.destructured
                val symbolStart = checkNotNull(match.groups[2]).range.first
                val line = source.text.substring(0, symbolStart).count { it == '\n' } + 1
                val origin = "${source.path}:$line"
                if ("#[unsafe(no_mangle)]" !in attributes) {
                    failures += "$origin: $symbol lacks #[unsafe(no_mangle)], so the JVM can't find it"
                }

                val types = params.split(',').map { it.substringAfter(':').trim() }.filter { it.isNotEmpty() }
                if (types.size < 2) {
                    failures += "$origin: $symbol must take (env, JClass) first, as a static native does"
                    continue
                }
                if (rustTypeName(types[0]) !in JNI_ENV_TYPES) {
                    failures += "$origin: $symbol takes `${types[0]}` where the JNIEnv pointer goes; " +
                        "expected one of $JNI_ENV_TYPES"
                }
                if (rustTypeName(types[1]) != "JClass") {
                    failures += "$origin: $symbol takes `${types[1]}` where a static native receives its JClass"
                }
                val args = types.drop(2).map { descriptorOfRust(it, descriptors, origin, failures) }
                val ret =
                    returnType.ifEmpty { null }?.let { descriptorOfRust(it, descriptors, origin, failures) } ?: "V"
                exports[symbol] = Native(args.joinToString("", prefix = "(", postfix = ")") + ret, origin)
            }
        }
        return exports
    }

    private fun descriptorOfRust(
        type: String,
        descriptors: Map<String, String>,
        origin: String,
        failures: MutableList<String>,
    ): String =
        descriptors[rustTypeName(type)] ?: run {
            failures += "$origin: no descriptor for Rust type `$type`; " +
                "add it to RUST_DESCRIPTORS if it maps to exactly one JVM type, " +
                "or bind the Java class with bind_java_type!"
            "?"
        }

    private fun rustTypeName(type: String): String = type.replace(GENERICS, "").substringAfterLast("::").trim()

    /** Every class compiled into this package, main and test alike. */
    private fun packageClasses(): List<Class<*>> {
        val loader = checkNotNull(javaClass.classLoader)
        val path = PACKAGE.replace('.', '/')
        return loader
            .getResources(path)
            .toList()
            .flatMap { url ->
                when (url.protocol) {
                    "file" -> {
                        val dir = File(url.toURI())
                        dir
                            .walkTopDown()
                            .filter { it.isFile && it.extension == "class" }
                            .map { "$path/" + it.relativeTo(dir).invariantSeparatorsPath }
                            .toList()
                    }

                    "jar" -> {
                        val jarPath = (url.openConnection() as JarURLConnection).jarFileURL.toURI()
                        JarFile(File(jarPath)).use { jar ->
                            jar
                                .entries()
                                .toList()
                                .map { it.name }
                                .filter { it.startsWith("$path/") && it.endsWith(".class") }
                        }
                    }

                    else -> {
                        throw AssertionError("can't list classes from $url")
                    }
                }
            }.map { it.removeSuffix(".class").replace('/', '.') }
            .distinct()
            .map { Class.forName(it, false, loader) }
    }

    /** The name the JVM looks up for [method]: JNI spec, "Resolving Native Method Names". */
    private fun jniSymbol(
        cls: Class<*>,
        method: Method,
        overloaded: Boolean,
    ): String {
        val short = "Java_" + jniMangle(cls.name.replace('.', '/')) + "_" + jniMangle(method.name)
        if (!overloaded) return short
        return short + "__" + jniMangle(descriptorOf(method).substringAfter('(').substringBefore(')'))
    }

    private fun jniMangle(name: String): String =
        buildString {
            for (c in name) {
                when (c) {
                    in 'a'..'z', in 'A'..'Z', in '0'..'9' -> append(c)
                    '/' -> append('_')
                    '_' -> append("_1")
                    ';' -> append("_2")
                    '[' -> append("_3")
                    else -> append("_0").append("%04x".format(c.code))
                }
            }
        }

    private companion object {
        const val PACKAGE = "org.jakebot.blew"

        val EXPORT_MARKER = Regex("""\bfn\s+Java_""")

        val EXPORT =
            Regex(
                """((?:#\[[^\]]*\]\s*)*)pub\s+(?:unsafe\s+)?extern\s+"(?:C|system)"\s+fn\s+(Java_\w+)\s*""" +
                    """\(([^)]*)\)\s*(?:->\s*([\w:<>']+)\s*)?\{""",
            )

        val GENERICS = Regex("<[^>]*>")

        val BINDING_MARKER = Regex("""\bbind_java_type!""")

        val BINDING =
            Regex("""\bbind_java_type!\s*\{\s*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*=>\s*"?([\w.$]+)"?\s*,?\s*\}""")

        /**
         * Wrappers that are ABI-identical to the raw `JNIEnv*` the JVM passes
         * first. Only add a type here if it is `#[repr(transparent)]` over it.
         */
        val JNI_ENV_TYPES = setOf("EnvUnowned")

        /**
         * Rust parameter types whose JVM type is unambiguous. `JObject` and
         * `JObjectArray` are deliberately absent: they fit many descriptors, so
         * a hook using one fails here rather than passing unchecked. Types the
         * workspace binds with `bind_java_type!` are added by [boundJavaTypes].
         */
        val RUST_DESCRIPTORS =
            mapOf(
                "jboolean" to "Z",
                "jbyte" to "B",
                "jchar" to "C",
                "jshort" to "S",
                "jint" to "I",
                "jlong" to "J",
                "jfloat" to "F",
                "jdouble" to "D",
                "JString" to "Ljava/lang/String;",
                "JBooleanArray" to "[Z",
                "JByteArray" to "[B",
                "JCharArray" to "[C",
                "JShortArray" to "[S",
                "JIntArray" to "[I",
                "JLongArray" to "[J",
                "JFloatArray" to "[F",
                "JDoubleArray" to "[D",
            )
    }
}
