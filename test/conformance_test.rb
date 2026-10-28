# frozen_string_literal: true

require_relative "test_helper"
require "open3"

class ConformanceTest < Minitest::Test
  CASES = {
    "quotes" => "printf '%s\\n' 'a b'",
    "parameter-default" => "unset X; printf '%s' \"${X:-ok}\"",
    "arithmetic" => "printf '%s' $((2 + 3 * 4))",
    "arithmetic-command" => "((1 + 2))",
    "brace-expansion" => "printf '%s ' a{b,c}",
    "arrays" => "a=(one two); printf '%s|%s' \"${a[1]}\" \"${#a[@]}\"",
    "if-else" => "if true; then printf yes; else printf no; fi",
    "for" => "for x in a b; do printf '%s' \"$x\"; done",
    "while" => "i=0; while [ \"$i\" -lt 2 ]; do printf '%s' \"$i\"; i=$((i + 1)); done",
    "case" => "case foo in f*) printf matched;; *) printf no;; esac",
    "function" => "f() { printf fn; }; f",
    "group-not" => "{ printf g; }; ! false",
    "here-doc" => "cat <<EOF\nhello\nEOF\n",
    "pipeline" => "printf 'a\\nb\\n' | wc -l",
    "redirection" => "printf hi > \"$CONFORMANCE_TMP\"; cat \"$CONFORMANCE_TMP\"",
    "double-bracket" => "[[ -n x && -z y ]]",
    "errexit" => "set -e; false; printf no",
    "trap-exit" => "trap 'printf bye' EXIT; printf hi",
    "command-substitution" => "printf '%s' \"$(printf sub)\""
  }.freeze

  def test_documented_subset_matches_bash
    Dir.mktmpdir("sharky-conformance-") do |dir|
      env = {
        "HOME" => @tmp,
        "PATH" => "/usr/bin:/bin",
        "LC_ALL" => "C.UTF-8",
        "CONFORMANCE_TMP" => File.join(dir, "out")
      }

      CASES.each do |name, command|
        bash_out, bash_err, bash_status = Open3.capture3(env, "bash", "--noprofile", "--norc", "-c", command)
        sharky_out, sharky_err, sharky_status = Open3.capture3(env, BIN, "--no-config", "-c", command)

        assert_equal bash_out, sharky_out, "#{name}: stdout differs (bash err: #{bash_err.inspect}, sharky err: #{sharky_err.inspect})"
        assert_equal bash_status.exitstatus, sharky_status.exitstatus, "#{name}: status differs"
      end
    end
  end
end
