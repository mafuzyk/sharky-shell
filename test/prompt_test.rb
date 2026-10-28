# frozen_string_literal: true

require_relative "test_helper"
require "sharky"

class PromptTest < Minitest::Test
  def test_render_uses_context
    ctx = Sharky::Prompt::Context.new(cwd: @tmp, home: @tmp, status: 1, duration_ms: 2500, git_branch: "main", git_dirty: true, ssh: false, user: "u", host: "h", jobs_count: 0, time: Time.now)
    out = Sharky::Prompt.render(ctx, theme: Sharky::Theme.new, prompt: ->(c) { "#{File.basename(c.cwd)} #{c.git_branch}#{c.git_dirty ? '*' : ''} #{c.status}" })
    assert_equal "#{File.basename(@tmp)} main* 1", out
  end

  def test_git_info_reads_branch_and_dirty
    Dir.chdir(@tmp) do
      system("git", "init", "-q")
      system("git", "config", "user.email", "t@example.com")
      system("git", "config", "user.name", "T")
      File.write("a.txt", "a")
      system("git", "add", "a.txt")
      system("git", "commit", "-qm", "init")
      File.write("a.txt", "b")
      info = Sharky::Prompt::Git.new(@tmp).info
      branch = `git -C #{@tmp} symbolic-ref --short HEAD`.strip
      assert_equal branch, info[:branch]
      assert_equal true, info[:dirty]
      assert_equal 0, info[:ahead]
      assert_equal 0, info[:behind]
      assert_equal 0, info[:staged]
      assert_equal 1, info[:modified]
      assert_equal 0, info[:untracked]
    end
  end

  def test_git_info_counts_ahead_behind_staged_modified_and_untracked
    remote = File.join(@tmp, "remote.git")
    clone = File.join(@tmp, "clone")
    repo = File.join(@tmp, "repo")
    git("init", "-q", "--bare", remote)
    git("init", "-q", repo)
    configure_git(repo)
    File.write(File.join(repo, "a.txt"), "a\n")
    git("-C", repo, "add", "a.txt")
    git("-C", repo, "commit", "-qm", "init")
    branch = `git -C #{repo} symbolic-ref --short HEAD`.strip
    git("--git-dir", remote, "symbolic-ref", "HEAD", "refs/heads/#{branch}")
    git("-C", repo, "remote", "add", "origin", remote)
    git("-C", repo, "push", "-q", "-u", "origin", branch)

    File.write(File.join(repo, "a.txt"), "local\n")
    git("-C", repo, "commit", "-qam", "local")
    git("clone", "-q", remote, clone)
    configure_git(clone)
    File.write(File.join(clone, "remote.txt"), "remote\n")
    git("-C", clone, "add", "remote.txt")
    git("-C", clone, "commit", "-qm", "remote")
    git("-C", clone, "push", "-q")
    git("-C", repo, "fetch", "-q", "origin")

    File.write(File.join(repo, "staged.txt"), "staged\n")
    git("-C", repo, "add", "staged.txt")
    File.write(File.join(repo, "a.txt"), "modified\n")
    File.write(File.join(repo, "untracked.txt"), "untracked\n")

    info = Sharky::Prompt::Git.new(repo).info
    assert_equal branch, info[:branch]
    assert_equal true, info[:dirty]
    assert_equal 1, info[:ahead]
    assert_equal 1, info[:behind]
    assert_equal 1, info[:staged]
    assert_equal 1, info[:modified]
    assert_equal 1, info[:untracked]
  end

  def test_git_info_clean_repository_has_zero_counters
    git("-C", @tmp, "init", "-q")
    configure_git(@tmp)
    File.write(File.join(@tmp, "a.txt"), "a\n")
    git("-C", @tmp, "add", "a.txt")
    git("-C", @tmp, "commit", "-qm", "init")
    branch = `git -C #{@tmp} symbolic-ref --short HEAD`.strip

    info = Sharky::Prompt::Git.new(@tmp).info
    assert_equal branch, info[:branch]
    assert_equal false, info[:dirty]
    assert_equal 0, info[:ahead]
    assert_equal 0, info[:behind]
    assert_equal 0, info[:staged]
    assert_equal 0, info[:modified]
    assert_equal 0, info[:untracked]
  end

  def test_git_info_uses_one_cached_status_query
    git_dir = File.join(@tmp, ".git")
    FileUtils.mkdir_p(git_dir)
    File.write(File.join(git_dir, "HEAD"), "ref: refs/heads/main\n")
    calls = File.join(@tmp, "calls")
    fake = File.join(@tmp, "git")
    File.write(fake, <<~SH)
      #!/bin/sh
      echo call >> #{calls.inspect}
      printf '%s\\n' '# branch.head main' '# branch.ab +2 -3' '1 M. N... 100644 100644 100644 a b f' '? new'
    SH
    File.chmod(0755, fake)

    first = Sharky::Prompt::Git.new(@tmp, git_command: fake).info
    second = Sharky::Prompt::Git.new(@tmp, git_command: fake).info

    assert_equal 1, File.readlines(calls).size
    assert_equal first, second
    assert_equal "main", first[:branch]
    assert_equal 2, first[:ahead]
    assert_equal 3, first[:behind]
    assert_equal 1, first[:staged]
  end

  def test_git_info_follows_worktree_gitdir_file
    git_dir = File.join(@tmp, "real-git")
    FileUtils.mkdir_p(File.join(git_dir, "refs", "heads"))
    File.write(File.join(git_dir, "HEAD"), "ref: refs/heads/feature/topic\n")
    worktree = File.join(@tmp, "worktree")
    FileUtils.mkdir_p(worktree)
    File.write(File.join(worktree, ".git"), "gitdir: #{git_dir}\n")

    info = Sharky::Prompt::Git.new(worktree).info
    assert_equal "feature/topic", info[:branch]
    assert_nil info[:dirty]
  end

  def test_git_info_without_repository
    info = Sharky::Prompt::Git.new(@tmp).info
    assert_nil info[:branch]
    assert_nil info[:dirty]
    assert_equal 0, info[:ahead]
    assert_equal 0, info[:behind]
    assert_equal 0, info[:staged]
    assert_equal 0, info[:modified]
    assert_equal 0, info[:untracked]
  end

  def test_slow_git_does_not_block_prompt
    FileUtils.mkdir_p(File.join(@tmp, ".git"))
    File.write(File.join(@tmp, ".git", "HEAD"), "ref: refs/heads/main\n")
    fake = File.join(@tmp, "git")
    pid_file = File.join(@tmp, "git.pid")
    File.write(fake, "#!/bin/sh\necho $$ > #{pid_file}\nexec sleep 1\n")
    File.chmod(0755, fake)
    started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    info = Sharky::Prompt::Git.new(@tmp, git_command: fake).info
    elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started
    assert_operator elapsed, :<, 0.3
    assert_nil info[:dirty]
    assert_equal 0, info[:ahead]
    assert_equal 0, info[:behind]
    assert_equal 0, info[:staged]
    assert_equal 0, info[:modified]
    assert_equal 0, info[:untracked]
  ensure
    if pid_file && File.exist?(pid_file)
      pid = File.read(pid_file).to_i
      begin
        Process.kill("KILL", pid)
        Process.wait(pid)
      rescue Errno::ESRCH, Errno::ECHILD
        nil
      end
    end
  end

  def test_prompt_exception_falls_back_to_default
    ctx = Sharky::Prompt::Context.new(cwd: @tmp, home: @tmp, status: 0, duration_ms: 0, git_branch: nil, git_dirty: nil, ssh: false, user: "u", host: "h", jobs_count: 0, time: Time.now)
    out = Sharky::Prompt.render(ctx, theme: Sharky::Theme.new, prompt: ->(_c) { raise "bad prompt" })
    assert_includes out, File.basename(@tmp)
  end

  def test_default_prompt_contains_context_segments
    old = ENV["NO_COLOR"]
    ENV["NO_COLOR"] = "1"
    ctx = Sharky::Prompt::Context.new(cwd: @tmp, home: @tmp, status: 1, duration_ms: 2500, git_branch: "main", git_dirty: true, ssh: true, user: "u", host: "h", jobs_count: 2, time: Time.now)
    out = Sharky::Prompt.render(ctx, theme: Sharky::Theme.new)
    assert_includes out, "u@h"
    assert_includes out, "~"
    assert_includes out, "main*"
    assert_includes out, "2.5s"
    assert_includes out, "[2]"
    assert_includes out, "❯"
  ensure
    ENV["NO_COLOR"] = old
  end

  def test_runtime_prompt_is_used_when_no_prompt_is_passed
    old = Sharky.runtime
    runtime = Sharky::Runtime.new
    runtime.prompt = ->(ctx) { "runtime #{ctx.status}" }
    Sharky.runtime = runtime
    ctx = Sharky::Prompt::Context.new(cwd: @tmp, home: @tmp, status: 7, duration_ms: 0, git_branch: nil, git_dirty: nil, ssh: false, user: "u", host: "h", jobs_count: 0, time: Time.now)
    assert_equal "runtime 7", Sharky::Prompt.render(ctx, theme: Sharky::Theme.new)
  ensure
    Sharky.runtime = old
  end

  def test_context_exposes_os_arch_and_git_counters
    ctx = Sharky::Prompt::Context.new(
      os: "Arch", arch: "x86_64",
      git_ahead: 2, git_behind: 3, git_staged: 4, git_modified: 5, git_untracked: 6
    )

    assert_equal "Arch", ctx.os
    assert_equal "x86_64", ctx.arch
    assert_equal 2, ctx.git_ahead
    assert_equal 3, ctx.git_behind
    assert_equal 4, ctx.git_staged
    assert_equal 5, ctx.git_modified
    assert_equal 6, ctx.git_untracked
  end

  def test_os_name_removes_quoted_linux_suffix
    path = File.join(@tmp, "os-release")
    File.write(path, "NAME=\"Arch Linux\"\n")

    assert_equal "Arch", Sharky::Prompt.os_name(path)
  end

  def test_os_name_reads_unquoted_name
    path = File.join(@tmp, "os-release")
    File.write(path, "NAME=CachyOS\n")

    assert_equal "CachyOS", Sharky::Prompt.os_name(path)
  end

  def test_arch_uses_host_cpu
    assert_equal RbConfig::CONFIG["host_cpu"], Sharky::Prompt.arch
  end

  private

  def git(*args)
    assert(system("git", *args), "git #{args.join(' ')} failed")
  end

  def configure_git(path)
    git("-C", path, "config", "user.email", "t@example.com")
    git("-C", path, "config", "user.name", "T")
  end
end
