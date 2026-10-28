require_relative "test_helper"
require "open3"

class InstallTest < Minitest::Test
  def test_install_run_and_uninstall
    prefix = File.join(@tmp, "prefix")
    out, err, status = Open3.capture3("make", "-s", "install", "PREFIX=#{prefix}", chdir: ROOT)
    assert status.success?, "install failed: #{out}#{err}"

    binary = File.join(prefix, "bin", "sharky")
    assert File.executable?(binary)
    assert File.file?(File.join(prefix, "lib", "sharky.rb"))
    assert File.file?(File.join(prefix, "lib", "sharky", "shell.rb"))
    assert File.file?(File.join(prefix, "share", "man", "man1", "sharky.1"))

    version, version_err, version_status = Open3.capture3(binary, "--version")
    assert version_status.success?, version_err
    assert_match(/sharky/, version)

    command, command_err, command_status = Open3.capture3(binary, "-c", "printf installed")
    assert command_status.success?, command_err
    assert_equal "installed", command

    _out, uninstall_err, uninstall_status = Open3.capture3("make", "-s", "uninstall", "PREFIX=#{prefix}", chdir: ROOT)
    assert uninstall_status.success?, uninstall_err
    refute File.exist?(binary)
    refute File.exist?(File.join(prefix, "lib", "sharky.rb"))
    refute File.exist?(File.join(prefix, "share", "man", "man1", "sharky.1"))
  end

  def test_arch_package_installs_from_the_cloned_repository_directory
    srcdir = File.join(@tmp, "src")
    source = File.join(srcdir, "sharky-shell")
    copy_checkout(source)
    pkgdir = File.join(@tmp, "package")

    out, err, status = Open3.capture3(
      { "srcdir" => srcdir, "pkgdir" => pkgdir },
      "bash", "-e", "-c", 'source "$1"; package', "package-test",
      File.join(source, "packaging", "PKGBUILD"), chdir: srcdir
    )

    assert status.success?, "Arch package failed: #{out}#{err}"
    binary = File.join(pkgdir, "usr", "bin", "sharky")
    assert File.executable?(binary)
    version, err, status = Open3.capture3(binary, "--version")
    assert status.success?, err
    assert_equal "sharky 1.0.1\n", version
  end

  def test_dist_install_check_preserves_existing_artifacts
    checkout = File.join(@tmp, "checkout")
    copy_checkout(checkout)
    artifact = File.join(checkout, "dist", "preexisting.tar.gz")
    FileUtils.mkdir_p(File.dirname(artifact))
    File.binwrite(artifact, "existing release artifact")

    out, err, status = Open3.capture3(
      RbConfig.ruby, "test/install_test.rb", "-n", "test_dist_tarball_installs", chdir: checkout
    )

    assert status.success?, "isolated dist install check failed: #{out}#{err}"
    assert File.file?(artifact), "the install check removed a preexisting dist artifact"
    assert_equal "existing release artifact", File.binread(artifact)
  end

  def test_dist_tarball_installs
    checkout = File.join(@tmp, "checkout")
    copy_checkout(checkout)
    _out, err, status = Open3.capture3("make", "-s", "dist", chdir: checkout)
    assert status.success?, "dist failed: #{err}"

    version = File.read(File.join(checkout, "lib", "sharky", "version.rb"))[/VERSION\s*=\s*"([^"]+)"/, 1]
    tarball = File.join(checkout, "dist", "sharky-#{version}.tar.gz")
    assert File.file?(tarball)

    _out, err, status = Open3.capture3("tar", "-xzf", tarball, "-C", @tmp)
    assert status.success?, "extract failed: #{err}"

    source = File.join(@tmp, "sharky-#{version}")
    %w[test/run.rb test/prompt_test.rb bench/prompt.rb bench/soak.rb].each do |entry|
      assert File.file?(File.join(source, entry)), "the source tarball is missing #{entry}"
    end
    prefix = File.join(@tmp, "prefix")
    _out, err, status = Open3.capture3("make", "-s", "install", "PREFIX=#{prefix}", chdir: source)
    assert status.success?, "dist install failed: #{err}"

    binary = File.join(prefix, "bin", "sharky")
    out, err, status = Open3.capture3(binary, "-c", "printf packaged")
    assert status.success?, err
    assert_equal "packaged", out
  end

  private

  def copy_checkout(target)
    FileUtils.mkdir_p(target)
    entries = %w[bin lib man packaging test bench .github README.md LICENSE CHANGELOG.md CONTRIBUTING.md SECURITY.md Makefile .editorconfig .gitattributes]
    FileUtils.cp_r(entries.map { |entry| File.join(ROOT, entry) }, target)
  end
end
