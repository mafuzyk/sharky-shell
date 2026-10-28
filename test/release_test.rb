require_relative "test_helper"
require "sharky"

class ReleaseTest < Minitest::Test
  def test_version_is_1_0_0
    assert_equal "1.0.0", Sharky::VERSION
    out, _err, status = run_sharky("--version")
    assert status.success?
    assert_equal "sharky 1.0.0\n", out
  end

  def test_changelog_has_release_entry_and_limitations
    changelog = File.read(File.join(ROOT, "CHANGELOG.md"))
    assert_includes changelog, "## [1.0.0]"
    assert_includes changelog, "Known limitations"
  end

  def test_man_and_pkgbuild_versions_match
    man = File.read(File.join(ROOT, "man", "sharky.1"))
    pkgbuild = File.read(File.join(ROOT, "packaging", "PKGBUILD"))
    assert_includes man, "sharky 1.0.0"
    assert_includes pkgbuild, "pkgver=1.0.0"
  end
end
