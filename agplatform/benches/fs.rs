use agbench::Bench;
use agplatform::Fs;
use agplatform::OpenWriteOptions;
use agplatform::Platform;
use agplatform::TestDir;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    agbench::main(&[
        exists,
        metadata,
        read_dir,
        open_read,
        open_write,
        create_dir_all,
        remove,
        copy,
        copy_dir_all,
        read,
        write,
        handle_read,
        handle_write,
    ])
}

/// A temporary directory set as the current directory with the platform
/// created in it, so that `std` and `agplatform` resolve the same relative
/// paths. Restores the current directory when dropped.
struct Fixture<P> {
    platform: P,
    current_dir: PathBuf,
    dir: TestDir,
}

fn fixture() -> Fixture<impl Platform> {
    let dir = TestDir::temp();
    let current_dir = std::env::current_dir().expect("current dir should be accessible");
    std::env::set_current_dir(dir.path()).expect("test dir should be enterable");

    Fixture {
        platform: agplatform::platform(),
        current_dir,
        dir,
    }
}

impl<P: Platform> Fixture<P> {
    fn fs(&self) -> &(impl Fs + use<P>) {
        self.platform.fs()
    }

    fn with_file(self, name: &str, len: usize) -> Self {
        self.fs()
            .write(name, vec![7_u8; len])
            .expect("file should be writable");
        self
    }
}

impl<P> Drop for Fixture<P> {
    fn drop(&mut self) {
        // WHY: Windows cannot remove the current directory.
        let _ = std::env::set_current_dir(&self.current_dir);
    }
}

fn exists(bench: &mut Bench) {
    bench
        .benchmark("fs/exists", || fixture().with_file("file", 64))
        .fail_factor(2.2)
        .target("std", |_| std::fs::exists("file").unwrap())
        .target("agplatform", |fx| fx.fs().exists("file").unwrap())
        .run();

    bench
        .benchmark("fs/exists/absolute", || {
            let fx = fixture().with_file("file", 64);
            let path = fx.dir.path().join("file");
            (fx, path)
        })
        .fail_factor(2.0)
        .target("std", |(_, path)| std::fs::exists(path).unwrap())
        .target("agplatform", |(fx, path)| fx.fs().exists(path).unwrap())
        .run();
}

fn metadata(bench: &mut Bench) {
    bench
        .benchmark("fs/metadata", || fixture().with_file("file", 64))
        .fail_factor(2.3)
        .target("std", |_| std::fs::metadata("file").unwrap())
        .target("agplatform", |fx| fx.fs().metadata("file").unwrap())
        .run();
}

fn read_dir(bench: &mut Bench) {
    bench
        .benchmark("fs/read_dir/16", || {
            let fx = fixture();
            fx.fs().create_dir_all("dir").unwrap();

            for i in 0..16 {
                fx.fs().write(format!("dir/{i}"), "").unwrap();
            }

            fx
        })
        .fail_factor(2.0)
        .target("std", |_| {
            std::fs::read_dir("dir")
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_type().unwrap().is_file())
                .count()
        })
        .target("agplatform", |fx| {
            fx.fs()
                .read_dir("dir")
                .unwrap()
                .filter(|e| e.as_ref().unwrap().file_type().is_file())
                .count()
        })
        .run();
}

fn open_read(bench: &mut Bench) {
    bench
        .benchmark("fs/open_read", || fixture().with_file("file", 64))
        .fail_factor(3.0)
        .target("std", |_| File::open("file").unwrap())
        .target("agplatform", |fx| fx.fs().open_read("file").unwrap())
        .run();
}

fn open_write(bench: &mut Bench) {
    bench
        .benchmark("fs/open_write", || fixture().with_file("file", 64))
        .fail_factor(3.0)
        .target("std", |_| {
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open("file")
                .unwrap()
        })
        .target("agplatform", |fx| fx.fs().open_write("file").unwrap())
        .run();
}

fn create_dir_all(bench: &mut Bench) {
    bench
        .benchmark("fs/create_dir_all/3", || (fixture(), 0))
        .fail_factor(1.5)
        .iteration_setup(|(_, count)| {
            *count += 1;
            format!("{count}/a/b")
        })
        .target("std", |_, path| std::fs::create_dir_all(path).unwrap())
        .target("agplatform", |(fx, _), path| {
            fx.fs().create_dir_all(path).unwrap()
        })
        .run();
}

fn remove(bench: &mut Bench) {
    bench
        .benchmark("fs/remove", || (fixture(), 0))
        .fail_factor(3.0)
        .iteration_setup(|(fx, count)| {
            *count += 1;
            let name = count.to_string();
            fx.fs().write(&name, "").unwrap();
            name
        })
        .target("std", |_, name| std::fs::remove_file(name).unwrap())
        .target("agplatform", |(fx, _), name| fx.fs().remove(name).unwrap())
        .run();
}

fn copy(bench: &mut Bench) {
    for (size, len, factor) in [("64B", 64, 2.5), ("64KiB", 64 << 10, 2.0)] {
        bench
            .benchmark(&format!("fs/copy/{size}"), || {
                (fixture().with_file("file", len), 0_u64)
            })
            .fail_factor(factor)
            .iteration_setup(|(_, count)| {
                *count += 1;

                // WHY: inputs are set up in batches ahead of their
                // iterations, only copies of earlier batches can be removed.
                if let Some(old) = count.checked_sub(256) {
                    let _ = std::fs::remove_file(old.to_string());
                }

                count.to_string()
            })
            .target("std", |_, name| std::fs::copy("file", name).unwrap())
            .target("agplatform", |(fx, _), name| {
                fx.fs().copy("file", name).unwrap()
            })
            .run();
    }
}

fn copy_dir_all(bench: &mut Bench) {
    /// Copies the tree of files and directories `source` to `target` like
    /// `Fs::copy_dir_all` does, setting the permissions of a directory once
    /// its content is copied.
    fn std_copy_dir_all(source: &Path, target: &Path) {
        std::fs::create_dir(target).unwrap();

        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let entry_target = target.join(entry.file_name());

            if entry.file_type().unwrap().is_dir() {
                std_copy_dir_all(&entry.path(), &entry_target);
            } else {
                std::fs::copy(entry.path(), entry_target).unwrap();
            }
        }

        let permissions = std::fs::metadata(source).unwrap().permissions();
        std::fs::set_permissions(target, permissions).unwrap();
    }

    bench
        .benchmark("fs/copy_dir_all/4x4", || {
            let fx = fixture();

            for dir in 0..4 {
                fx.fs().create_dir_all(format!("tree/{dir}")).unwrap();

                for file in 0..4 {
                    fx.fs()
                        .write(format!("tree/{dir}/{file}"), [7_u8; 64])
                        .unwrap();
                }
            }

            (fx, 0_u64)
        })
        .fail_factor(2.0)
        .iteration_setup(|(_, count)| {
            *count += 1;

            // WHY: see `copy`.
            if let Some(old) = count.checked_sub(256) {
                let _ = std::fs::remove_dir_all(old.to_string());
            }

            count.to_string()
        })
        .target("std", |_, name| {
            std_copy_dir_all(Path::new("tree"), Path::new(&name))
        })
        .target("agplatform", |(fx, _), name| {
            fx.fs().copy_dir_all("tree", name).unwrap()
        })
        .run();
}

fn read(bench: &mut Bench) {
    for (size, len, factor) in [
        ("64B", 64, 2.5),
        ("64KiB", 64 << 10, 3.0),
        ("4MiB", 4 << 20, 2.5),
    ] {
        bench
            .benchmark(&format!("fs/read/{size}"), || {
                fixture().with_file("file", len)
            })
            .fail_factor(factor)
            .target("std", |_| std::fs::read("file").unwrap())
            .target("agplatform", |fx| fx.fs().read("file").unwrap())
            .run();
    }
}

fn write(bench: &mut Bench) {
    for (size, len, factor) in [
        ("64B", 64, 2.2),
        ("64KiB", 64 << 10, 1.5),
        ("4MiB", 4 << 20, 1.5),
    ] {
        bench
            .benchmark(&format!("fs/write/{size}"), || (fixture(), vec![7_u8; len]))
            .fail_factor(factor)
            .target("std", |(_, data)| std::fs::write("file", data).unwrap())
            .target("agplatform", |(fx, data)| {
                fx.fs().write("file", data).unwrap()
            })
            .run();
    }
}

fn handle_read(bench: &mut Bench) {
    /// Reads `buf.len()` bytes, starting over at the end of the file.
    fn read_cycling<F: Read + Seek>(file: &mut F, buf: &mut [u8]) {
        if file.read(buf).unwrap() == 0 {
            file.rewind().unwrap();
        }
    }

    bench
        .benchmark("fs/handle/read/64B", || {
            let fx = fixture().with_file("file", 64 << 10);
            let std = File::open("file").unwrap();
            let ag = fx.fs().open_read("file").unwrap();
            (std, ag, [0_u8; 64], fx)
        })
        .fail_factor(1.2)
        .target("std", |(std, _, buf, _)| read_cycling(std, buf))
        .target("agplatform", |(_, ag, buf, _)| read_cycling(ag, buf))
        .run();
}

fn handle_write(bench: &mut Bench) {
    /// Writes 64 bytes, starting over once `written` reaches 64 KiB so that
    /// the file does not grow without bound.
    fn write_cycling<F: Write + Seek>(file: &mut F, written: &mut usize) {
        file.write_all(&[7_u8; 64]).unwrap();
        *written += 64;

        if *written >= 64 << 10 {
            file.rewind().unwrap();
            *written = 0;
        }
    }

    bench
        .benchmark("fs/handle/write/64B", || {
            let fx = fixture();
            let std = File::create("std").unwrap();
            let ag = fx
                .fs()
                .open_write_options("ag", OpenWriteOptions::new().create().truncate())
                .unwrap();
            (std, ag, 0, 0, fx)
        })
        .fail_factor(1.2)
        .target("std", |(std, _, written, _, _)| write_cycling(std, written))
        .target("agplatform", |(_, ag, _, written, _)| {
            write_cycling(ag, written)
        })
        .run();
}
