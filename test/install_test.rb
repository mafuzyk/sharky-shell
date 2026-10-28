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

  def test_dist_tarball_installs
    _out, err, status = Open3.capture3("make", "-s", "dist", chdir: ROOT)
    assert status.success?, "dist failed: #{err}"

    version = File.read(File.join(ROOT, "lib", "sharky", "version.rb"))[/VERSION\s*=\s*"([^"]+)"/, 1]
    tarball = File.join(ROOT, "dist", "sharky-#{version}.tar.gz")
    assert File.file?(tarball)

    Dir.mktmpdir("sharky-dist-") do |dir|
      _out, err, status = Open3.capture3("tar", "-xzf", tarball, "-C", dir)
      assert status.success?, "extract failed: #{err}"

      source = File.join(dir, "sharky-#{version}")
      prefix = File.join(dir, "prefix")
      _out, err, status = Open3.capture3("make", "-s", "install", "PREFIX=#{prefix}", chdir: source)
      assert status.success?, "dist install failed: #{err}"

      binary = File.join(prefix, "bin", "sharky")
      out, err, status = Open3.capture3(binary, "-c", "printf packaged")
      assert status.success?, err
      assert_equal "packaged", out
    end
  ensure
    FileUtils.rm_rf(File.join(ROOT, "dist"))
  end
end
