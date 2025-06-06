mod codeview;
mod debug;
mod dwarf;
mod gimli_writer;
mod x64;

use crate::common::local_env::*;
use crate::common::node::*;
use debug::*;
use object::{write::*, *};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::*;
use std::ffi::OsStr;
use std::io::Write;
use std::path::*;

// Names for special symbol

// Prototype symbols for primitive types
const BOOL_PROTOTYPE: &str = "bool.$proto";
const INT_PROTOTYPE: &str = "int.$proto";
const STR_PROTOTYPE: &str = "str.$proto";
const BOOL_LIST_PROTOTYPE: &str = "[bool].$proto";
const INT_LIST_PROTOTYPE: &str = "[int].$proto";
const OBJECT_LIST_PROTOTYPE: &str = "[object].$proto";

// Standard library function symboles
const BUILTIN_ALLOC_OBJ: &str = "$alloc_obj";
const BUILTIN_DIV_ZERO: &str = "$div_zero";
const BUILTIN_OUT_OF_BOUND: &str = "$out_of_bound";
const BUILTIN_NONE_OP: &str = "$none_op";
const BUILTIN_LEN: &str = "$len";
const BUILTIN_INPUT: &str = "$input";
const BUILTIN_PRINT: &str = "$print";
const BUILTIN_INIT: &str = "$init";

// Program entry point symbol
const BUILTIN_CHOCOPY_MAIN: &str = "$chocopy_main";

// Special data section symbols
const GLOBAL_SECTION: &str = "$global";
const INIT_PARAM: &str = "$init_param";

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum Platform {
    Windows,
    Linux,
    Macos,
}

/// Type for debug info
///
/// Example: `[[[str]]]` will be `TypeDebug { core_name: "str", array_level: 3 }`
#[derive(PartialEq, Eq, Hash, Clone)]
struct TypeDebug {
    core_name: String,
    array_level: u32,
}

/// Represents a group of types for debug info
///
/// Example: `TypeDebug { core_name: "str", max_array_levell: 3 }` represents
/// `str`, `[str]`, `[[str]]` and `[[[str]]]`.
struct TypeDebugRepresentive<'a> {
    core_name: &'a str,
    max_array_level: u32,
}

impl TypeDebug {
    /// Construct a non-list type
    fn class_type(name: &str) -> TypeDebug {
        TypeDebug {
            core_name: name.to_owned(),
            array_level: 0,
        }
    }
    fn from_annotation(type_annotation: &TypeAnnotation) -> TypeDebug {
        match type_annotation {
            TypeAnnotation::ClassType(c) => TypeDebug {
                core_name: c.class_name.clone(),
                array_level: 0,
            },
            TypeAnnotation::ListType(l) => {
                let mut type_debug = TypeDebug::from_annotation(&l.element_type);
                type_debug.array_level += 1;
                type_debug
            }
        }
    }
}

impl std::fmt::Display for TypeDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for _ in 0..self.array_level {
            write!(f, "[")?;
        }
        write!(f, "{}", &self.core_name)?;
        for _ in 0..self.array_level {
            write!(f, "]")?;
        }
        Ok(())
    }
}

// Variable info for debug info
#[derive(Clone)]
struct VarDebug {
    // Offset relative to some base address. The base address is
    //  - for global variable: global section start.
    //  - for local variable: rbp.
    //  - for class attribute: object address.
    offset: i32,
    line: u32, // Line number in source code that defines the variable
    name: String,
    var_type: TypeDebug,
}

// Relates machine code with source code
struct LineMap {
    code_pos: usize,  // Location in machine code
    line_number: u32, // Line number in source code
}

// Procedure info for debug info
struct ProcedureDebug {
    decl_line: u32,         // Line number is source code that defines the function
    artificial: bool,       // true if the procedure is not defined in source code
    parent: Option<String>, // Name of parent function
    lines: Vec<LineMap>,    // Maps machine code and source code
    return_type: TypeDebug,
    params: Vec<VarDebug>,
    locals: Vec<VarDebug>,
    frame_size: u32, // Stack frame size, excluding saved ret and rbp.
}

impl ProcedureDebug {
    // Collect all types appeared in this function
    fn used_types(&self) -> impl Iterator<Item = &TypeDebug> {
        std::iter::once(&self.return_type)
            .chain(self.params.iter().map(|param| &param.var_type))
            .chain(self.locals.iter().map(|local| &local.var_type))
    }
}

// Extra info for a chunk
enum ChunkExtra {
    Procedure(ProcedureDebug), // An executable chunk with debug info
    Data { writable: bool },   // A data chunk that can be writable or read-only
}

// The target of a relocation
enum ChunkLinkTarget {
    Symbol(String, i32), // Relocation by symbol name and addend
    Data(Vec<u8>),       // Create an ad hoc small chunk and make it the target
}

// Relocation between chunks
struct ChunkLink {
    pos: usize, // Relocation source location
    to: ChunkLinkTarget,
}

// A piece of data with a symbol name
struct Chunk {
    name: String,          // Symbol name
    code: Vec<u8>,         // Data content
    links: Vec<ChunkLink>, // Relocations from this chunk
    extra: ChunkExtra,
}

// Relocation type for debug chunk
enum DebugChunkLinkType {
    Absolute,
    // The types below are Windows-specific
    SectionRelative,
    SectionId,
    ImageRelative,
}

// Relocation between chunks, specifically when the source chunk is debug info
struct DebugChunkLink {
    link_type: DebugChunkLinkType,
    pos: usize, // Source location
    to: String, // Target symbol
    size: u8,   // in bytes
}

// Chunk for debug info
struct DebugChunk {
    name: String,               // Section name
    code: Vec<u8>,              // Data content
    links: Vec<DebugChunkLink>, // Relocations from this chunk
    discardable: bool,          // true in general but false for exception handling info.
}

// Method type info for debug info
#[derive(Clone, PartialEq, Eq, Hash)]
struct MethodDebug {
    params: Vec<TypeDebug>,
    return_type: TypeDebug,
}

// Class info for debug info
#[derive(Clone)]
struct ClassDebug {
    size: u32, // Object size, excluding the object header
    attributes: Vec<VarDebug>,
    methods: BTreeMap<u32, (String, MethodDebug)>, // Map from prototype offset to (name, type)
}

impl ClassDebug {
    // Collect all types appeared in this class
    fn used_types(&self) -> impl Iterator<Item = &TypeDebug> {
        self.attributes.iter().map(|attribute| &attribute.var_type)
    }
}

// The generated ChocoPy program, without linking to other libraries
struct CodeSet {
    chunks: Vec<Chunk>,
    global_size: u64,             // Section size reserved for all global variables
    globals_debug: Vec<VarDebug>, // Debug info for global variables
    classes_debug: HashMap<String, ClassDebug>,
}

impl CodeSet {
    // Collect all types appeared in the program
    fn used_types(&self) -> impl Iterator<Item = &TypeDebug> {
        self.chunks
            .iter()
            .filter_map(|chunk| {
                if let ChunkExtra::Procedure(procedure) = &chunk.extra {
                    Some(procedure.used_types())
                } else {
                    None
                }
            })
            .flatten()
            .chain(self.globals_debug.iter().map(|global| &global.var_type))
            .chain(
                self.classes_debug
                    .iter()
                    .flat_map(|(_, class)| class.used_types()),
            )
    }

    // Collects all types appeared in the program, and returns representives
    // that have the highest array level.
    fn used_types_representive(&self) -> impl Iterator<Item = TypeDebugRepresentive> {
        let mut array_level_map = HashMap::<&str, u32>::new();
        for type_used in self.used_types() {
            if let Some(array_level) = array_level_map.get_mut(type_used.core_name.as_str()) {
                *array_level = std::cmp::max(*array_level, type_used.array_level)
            } else {
                array_level_map.insert(&type_used.core_name, type_used.array_level);
            }
        }
        array_level_map.entry("int").or_insert(0);
        array_level_map.entry("str").or_insert(0);
        array_level_map.entry("bool").or_insert(0);
        array_level_map.entry("object").or_insert(0);
        array_level_map.entry("<None>").or_insert(0);
        array_level_map
            .into_iter()
            .map(|(core_name, max_array_level)| TypeDebugRepresentive {
                core_name,
                max_array_level,
            })
    }
}

#[derive(Debug)]
struct ToolChainError;

impl std::fmt::Display for ToolChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Failed to find MSVC tools. Please install Visual Studio or Visual C++ Build Tools"
        )
    }
}

impl std::error::Error for ToolChainError {}

#[derive(Debug)]
pub struct PathError;

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Illegal path")
    }
}

impl std::error::Error for PathError {}

// Encode path string for Windows command line argument
fn windows_path_escape(path: &Path) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let path = path.to_str().ok_or(PathError)?;

    // TODO: actually escape the path
    // For now we just forbid suspicious strings.
    if path
        .find(|c| matches!(c, '\"' | '\'' | '^') || c.is_control())
        .is_some()
        || path.ends_with('\\')
    {
        return Err(PathError.into());
    }

    Ok(path.to_owned())
}

// Generate object file
pub fn gen_object(
    source_path: &str,
    ast: Program,
    obj_path: &Path,
    platform: Platform,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let current_dir_buf = std::env::current_dir();
    let current_dir = current_dir_buf
        .as_ref()
        .map(|s| s.to_str())
        .ok()
        .flatten()
        .unwrap_or("");

    // Debug section generator
    let mut debug: Box<dyn DebugWriter> = match platform {
        Platform::Windows => Box::new(codeview::Codeview::new(
            source_path,
            current_dir,
            obj_path.as_os_str().to_str().unwrap_or(""),
        )?),
        Platform::Linux => Box::new(dwarf::Dwarf::new(
            dwarf::DwarfFlavor::Linux,
            source_path,
            current_dir,
        )),
        Platform::Macos => Box::new(dwarf::Dwarf::new(
            dwarf::DwarfFlavor::Macos,
            source_path,
            current_dir,
        )),
    };

    let binary_format = match platform {
        Platform::Windows => BinaryFormat::Coff,
        Platform::Linux => BinaryFormat::Elf,
        Platform::Macos => BinaryFormat::MachO,
    };

    // Object file generator
    let mut obj = Object::new(binary_format, Architecture::X86_64, Endianness::Little);

    // Import standard library functions
    let import_function = |obj: &mut Object, name: &str| {
        obj.add_symbol(Symbol {
            name: name.into(),
            value: 0,
            size: 0,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Undefined,
            flags: SymbolFlags::None,
        })
    };

    import_function(&mut obj, BUILTIN_ALLOC_OBJ);
    import_function(&mut obj, BUILTIN_DIV_ZERO);
    import_function(&mut obj, BUILTIN_OUT_OF_BOUND);
    import_function(&mut obj, BUILTIN_NONE_OP);
    import_function(&mut obj, BUILTIN_LEN);
    import_function(&mut obj, BUILTIN_PRINT);
    import_function(&mut obj, BUILTIN_INPUT);
    import_function(&mut obj, BUILTIN_INIT);

    // Generate machine code and debug info
    let code_set = x64::gen_code_set(ast, platform);

    // Feed type/class debug info to debug section generator
    for t in code_set.used_types_representive() {
        debug.add_type(t);
    }

    for (class_name, classes_debug) in code_set.classes_debug {
        debug.add_class(class_name, classes_debug);
    }

    // Allocate section for global variables
    let bss_section = obj.section_id(StandardSection::UninitializedData);

    let global_symbol = obj.add_symbol(Symbol {
        name: GLOBAL_SECTION.into(),
        value: 0,
        size: code_set.global_size,
        kind: SymbolKind::Data,
        scope: SymbolScope::Compilation,
        weak: false,
        section: SymbolSection::Undefined,
        flags: SymbolFlags::None,
    });

    obj.add_symbol_bss(global_symbol, bss_section, code_set.global_size, 8);

    // Feed global variable debug info to debug section generator
    for global_debug in code_set.globals_debug {
        debug.add_global(global_debug);
    }

    // Feed chunks to object file

    // Map between section name and location for use in relocation later
    let mut section_map = HashMap::new();

    // Sections that chunks can feed to
    let text_section = obj.section_id(StandardSection::Text);
    let data_section = obj.section_id(StandardSection::Data);
    let ro_section = obj.section_id(StandardSection::ReadOnlyData);
    let ro_reloc_section = obj.section_id(StandardSection::ReadOnlyDataWithRel);

    for chunk in &code_set.chunks {
        debug.add_chunk(chunk); // Feed the chunk debug info to debug section generator

        // Select section attributes for this chunk
        let section;
        let align;
        let kind;
        match chunk.extra {
            ChunkExtra::Procedure(_) => {
                section = text_section;
                align = 1;
                kind = SymbolKind::Text;
            }
            ChunkExtra::Data { writable } => {
                section = if writable {
                    data_section
                } else if chunk.links.is_empty() {
                    ro_section
                } else {
                    ro_reloc_section
                };
                align = 8;
                kind = SymbolKind::Data;
            }
        }

        // Only the entry point is exposed in linkage scope for linking with external entry point
        let scope = if chunk.name == BUILTIN_CHOCOPY_MAIN {
            SymbolScope::Linkage
        } else {
            SymbolScope::Compilation
        };

        let offset = obj.append_section_data(section, &chunk.code, align);
        obj.add_symbol(Symbol {
            name: chunk.name.as_bytes().into(),
            value: offset,
            size: chunk.code.len() as u64,
            kind,
            scope,
            weak: false,
            section: SymbolSection::Section(section),
            flags: SymbolFlags::None,
        });
        section_map.insert(&chunk.name, (section, offset));
    }

    // Add relocations

    let mut data_id = 0;

    for chunk in &code_set.chunks {
        let (from, from_offset) = section_map[&chunk.name];
        let size;
        let kind;
        let encoding;
        let addend;
        if let ChunkExtra::Procedure(_) = chunk.extra {
            size = 32;
            kind = RelocationKind::Relative;
            encoding = RelocationEncoding::X86RipRelative;
            addend = -4;
        } else {
            size = 64;
            kind = RelocationKind::Absolute;
            encoding = RelocationEncoding::Generic;
            addend = 0;
        };
        for link in &chunk.links {
            let (symbol, symbol_addend) = match &link.to {
                ChunkLinkTarget::Symbol(symbol, addend) => {
                    (obj.symbol_id(symbol.as_bytes()).unwrap(), *addend)
                }
                ChunkLinkTarget::Data(data) => {
                    let name = format!("$str{}", data_id);
                    data_id += 1;
                    let offset = obj.append_section_data(ro_section, data, 1);

                    (
                        obj.add_symbol(Symbol {
                            name: name.into(),
                            value: offset,
                            size: 0,
                            kind: SymbolKind::Data,
                            scope: SymbolScope::Compilation,
                            weak: false,
                            section: SymbolSection::Section(ro_section),
                            flags: SymbolFlags::None,
                        }),
                        0,
                    )
                }
            };
            obj.add_relocation(
                from,
                Relocation {
                    offset: from_offset + link.pos as u64,
                    size,
                    kind,
                    encoding,
                    symbol,
                    addend: addend + symbol_addend as i64,
                },
            )?;
        }
    }

    // Finalize debug section generation and feed them to the object file

    let debug_chunks = debug.finalize();
    let mut debug_section_map = HashMap::new();
    for chunk in &debug_chunks {
        let kind = if chunk.discardable {
            SectionKind::Debug
        } else {
            SectionKind::ReadOnlyData
        };
        let section = obj.add_section(
            obj.segment_name(StandardSegment::Debug).into(),
            chunk.name.as_bytes().into(),
            kind,
        );
        obj.append_section_data(section, &chunk.code, 8);
        debug_section_map.insert(chunk.name.clone(), section);
    }

    // .. as well as their relocations

    for chunk in debug_chunks {
        for link in chunk.links {
            let to = obj
                .symbol_id(link.to.as_bytes())
                .unwrap_or_else(|| obj.section_symbol(debug_section_map[&link.to]));
            let kind = match link.link_type {
                DebugChunkLinkType::Absolute => RelocationKind::Absolute,
                DebugChunkLinkType::SectionRelative => RelocationKind::SectionOffset,
                DebugChunkLinkType::SectionId => RelocationKind::SectionIndex,
                DebugChunkLinkType::ImageRelative => RelocationKind::ImageOffset,
            };
            obj.add_relocation(
                debug_section_map[&chunk.name],
                Relocation {
                    offset: link.pos as u64,
                    size: link.size * 8,
                    kind,
                    encoding: RelocationEncoding::Generic,
                    symbol: to,
                    addend: 0,
                },
            )?;
        }
    }

    // Output the object file
    let mut obj_file = std::fs::File::create(obj_path)?;
    obj_file.write_all(&obj.write()?)?;

    Ok(())
}

// Link the object file with libraries to produce an executable
pub fn link(
    obj_path: &Path,
    path: &str,
    static_lib: bool, // prefer static library instead of dynamic library
    platform: Platform,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Find the standard library
    let lib_file = match platform {
        Platform::Windows => "chocopy_stdlib.lib",
        Platform::Linux | Platform::Macos => "libchocopy_stdlib.a",
    };

    let mut lib_path = std::env::current_exe()?;
    lib_path.set_file_name(lib_file);

    // Invoke the linker
    let ld_output = match platform {
        Platform::Windows => {
            let vcvarsall = (|| -> Option<PathBuf> {
                let linker = cc::windows_registry::find_tool("x86_64-pc-windows-msvc", "link.exe")?;
                Some(
                    linker
                        .path()
                        .ancestors()
                        .nth(7)?
                        .join("Auxiliary")
                        .join("Build")
                        .join("vcvarsall.bat"),
                )
            })()
            .ok_or(ToolChainError)?;

            let libs = if static_lib {
                "libvcruntime.lib libucrt.lib libcmt.lib"
            } else {
                "vcruntime.lib ucrt.lib msvcrt.lib"
            };

            // We need to execute vcvarsall.bat, then link.exe with the
            // inherited environment variables.
            // However, the syntax for chained execution in `cmd` is not in the
            // standard escaping format, and rust std::process::Command doesn't
            // support it. To work around this, we make a temporary batch file
            // with the commands we want, and execute that batch file.
            let batch_content = format!(
                "@echo off
    call \"{}\" amd64
    link /NOLOGO /NXCOMPAT /OPT:REF,NOICF \
    \"{}\" \"{}\" /OUT:\"{}\" \
    kernel32.lib advapi32.lib ws2_32.lib userenv.lib Bcrypt.lib ntdll.lib {} \
    /SUBSYSTEM:CONSOLE /DEBUG",
                windows_path_escape(&vcvarsall)?,
                windows_path_escape(obj_path)?,
                windows_path_escape(&lib_path)?,
                windows_path_escape(Path::new(path))?,
                libs
            );

            let mut bat_path = std::env::temp_dir();
            let bat_name = format!("chocopy-{}.bat", rand::random::<u32>());
            bat_path.push(bat_name);

            std::fs::write(&bat_path, batch_content)?;

            let ld_output = std::process::Command::new("cmd")
                .args([OsStr::new("/c"), bat_path.as_os_str()])
                .output()?;
            std::fs::remove_file(&bat_path)?;
            ld_output
        }
        Platform::Linux | Platform::Macos => {
            let mut command = std::process::Command::new("cc");
            command.args([
                OsStr::new("-arch"),
                OsStr::new("x86_64"),
                OsStr::new("-o"),
                OsStr::new(path),
                obj_path.as_os_str(),
                lib_path.as_os_str(),
                OsStr::new("-pthread"),
                OsStr::new("-ldl"),
            ]);
            if static_lib {
                command.arg("-static");
            }
            command.output()?
        }
    };

    if !ld_output.status.success() {
        eprintln!("Error: Linker returned {}", ld_output.status);
        if !ld_output.stdout.is_empty() {
            eprintln!("STDOUT from linker:");
            std::io::stderr().write_all(&ld_output.stdout).unwrap();
        }
        if !ld_output.stderr.is_empty() {
            eprintln!("STDERR from linker:");
            std::io::stderr().write_all(&ld_output.stderr).unwrap();
        }
    }

    Ok(())
}

// Generates object file or executable
pub fn codegen(
    source_path: &str,
    ast: Program,
    path: &str,
    no_link: bool,
    static_lib: bool,
    platform: Platform,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let obj_path = if no_link {
        let obj_path = Path::new(path);
        obj_path.to_owned()
    } else {
        let mut obj_path = std::env::temp_dir();
        let obj_name = format!("chocopy-{}.o", rand::random::<u32>());
        obj_path.push(obj_name);
        obj_path
    };

    gen_object(source_path, ast, &obj_path, platform)?;

    if no_link {
        return Ok(());
    }

    link(&obj_path, path, static_lib, platform)?;

    std::fs::remove_file(&obj_path)?;

    Ok(())
}
