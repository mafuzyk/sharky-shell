# frozen_string_literal: true

require "rbconfig"
require_relative "theme"

module Sharky
  module Prompt
    DURATION_THRESHOLD_MS = 2000

    Context = Struct.new(
      :cwd, :home, :status, :duration_ms, :git_branch, :git_dirty,
      :git_ahead, :git_behind, :git_staged, :git_modified, :git_untracked,
      :os, :arch, :ssh, :user, :host, :jobs_count, :time,
      keyword_init: true
    ) do
      def ssh?
        ssh
      end
    end

    class Git
      TIMEOUT_SECONDS = 0.05
      CACHE_SECONDS = 0.2

      class << self
        def cache
          @cache ||= {}
        end
      end

      def initialize(cwd, git_command: "git")
        @cwd = File.expand_path(cwd)
        @git_command = git_command
      end

      def info
        git_dir = find_git_dir
        return empty_info unless git_dir

        cached_info(git_dir)
      end

      private

      def empty_info(branch = nil, dirty = nil)
        {
          branch: branch,
          dirty: dirty,
          ahead: 0,
          behind: 0,
          staged: 0,
          modified: 0,
          untracked: 0
        }
      end

      def cached_info(git_dir)
        key = [git_dir, index_mtime(git_dir)]
        cached = self.class.cache[key]
        return cached[:info] if cached && (monotonic - cached[:at]) < CACHE_SECONDS

        info = query_info(git_dir)
        self.class.cache[key] = { at: monotonic, info: info }
        info
      end

      def index_mtime(git_dir)
        File.mtime(File.join(git_dir, "index")).to_f
      rescue SystemCallError
        nil
      end

      def query_info(git_dir)
        info = query_status
        return empty_info(read_branch(git_dir)) unless info

        info[:branch] ||= read_branch(git_dir)
        info
      end

      def find_git_dir
        directory = @cwd
        loop do
          candidate = File.join(directory, ".git")
          return candidate if File.directory?(candidate)

          if File.file?(candidate)
            line = File.readlines(candidate).first.to_s.strip
            if line.start_with?("gitdir:")
              target = line.delete_prefix("gitdir:").strip
              return File.expand_path(target, directory)
            end
          end

          parent = File.dirname(directory)
          return nil if parent == directory

          directory = parent
        end
      rescue SystemCallError
        nil
      end

      def read_branch(git_dir)
        head = File.read(File.join(git_dir, "HEAD")).strip
        prefix = "ref: refs/heads/"
        return head.delete_prefix(prefix) if head.start_with?(prefix)

        nil
      rescue SystemCallError
        nil
      end

      def query_status
        deadline = monotonic + TIMEOUT_SECONDS
        out_reader, out_writer = IO.pipe
        err_reader, err_writer = IO.pipe
        pid = Process.spawn(
          { "GIT_OPTIONAL_LOCKS" => "0" },
          @git_command, "status", "--porcelain=v2", "--branch",
          chdir: @cwd, out: out_writer, err: err_writer, pgroup: true
        )
        out_writer.close
        err_writer.close

        out_thread = Thread.new { out_reader.read }
        err_thread = Thread.new { err_reader.read }
        status = wait_for(pid, deadline)
        return nil unless status&.success?

        [out_thread, err_thread].each do |thread|
          remaining = deadline - monotonic
          unless remaining.positive? && thread.join(remaining)
            kill_process_group(pid)
            return nil
          end
        end
        parse_status(out_thread.value)
      rescue SystemCallError, ArgumentError
        nil
      ensure
        [out_thread, err_thread].compact.each { |thread| thread.kill.join }
        close_io(out_reader)
        close_io(out_writer)
        close_io(err_reader)
        close_io(err_writer)
      end

      def parse_status(output)
        info = empty_info(nil, false)
        output.each_line do |raw|
          line = raw.chomp
          case line
          when /\A# branch\.head (.+)\z/
            head = Regexp.last_match(1).strip
            info[:branch] = head unless head == "(detached)"
          when /\A# branch\.ab \+(\d+) -(\d+)\z/
            info[:ahead] = Regexp.last_match(1).to_i
            info[:behind] = Regexp.last_match(2).to_i
          when /\A[12u] (.)(.) /
            info[:staged] += 1 unless Regexp.last_match(1) == "."
            info[:modified] += 1 unless Regexp.last_match(2) == "."
          when /\A\? /
            info[:untracked] += 1
          end
        end
        info[:dirty] = info[:staged].positive? || info[:modified].positive?
        info
      end

      def wait_for(pid, deadline)
        loop do
          result = Process.waitpid2(pid, Process::WNOHANG)
          return result[1] if result

          if monotonic >= deadline
            kill_process_group(pid)
            return nil
          end
          sleep 0.001
        end
      end

      def kill_process_group(pid)
        Process.kill("KILL", -pid)
      rescue Errno::ESRCH, Errno::EPERM
        nil
      ensure
        begin
          Process.waitpid(pid)
        rescue Errno::ECHILD
          nil
        end
      end

      def close_io(io)
        io&.close unless io&.closed?
      rescue IOError
        nil
      end

      def monotonic
        Process.clock_gettime(Process::CLOCK_MONOTONIC)
      end
    end

    module_function

    def os_name(path = "/etc/os-release")
      line = File.foreach(path).find { |entry| entry.start_with?("NAME=") }
      return nil unless line

      value = line.delete_prefix("NAME=").strip
      if value.length >= 2 && (value[0] == '"' || value[0] == "'") && value[0] == value[-1]
        value = value[1...-1]
      end
      value.delete_suffix(" Linux")
    rescue SystemCallError
      nil
    end

    def arch
      RbConfig::CONFIG["host_cpu"]
    end

    def render(context, theme: Theme.new, prompt: nil)
      callable = prompt || runtime_prompt
      if callable
        begin
          return callable.call(context).to_s
        rescue StandardError => error
          warn_once(error)
          fallback_context = context.dup
          fallback_context.home = nil if fallback_context.respond_to?(:home=)
          return default_prompt(fallback_context, theme)
        end
      end

      default_prompt(context, theme)
    end

    def runtime_prompt
      return nil unless Sharky.respond_to?(:runtime)

      Sharky.runtime&.prompt
    end

    def default_prompt(context, theme)
      segments = []
      segments << theme.style(:hint, "#{context.user}@#{context.host}") if context.ssh?
      segments << theme.style(:directory, shorten_cwd(context.cwd, context.home))

      if context.git_branch && !context.git_branch.empty?
        branch = context.git_dirty ? "#{context.git_branch}*" : context.git_branch
        segments << theme.style(:git, branch)
      end

      duration = format_duration(context.duration_ms)
      segments << theme.style(:hint, duration) if duration

      if context.jobs_count.to_i.positive?
        segments << theme.style(:hint, "[#{context.jobs_count}]")
      end

      style = context.status.to_i.zero? ? :success : :error
      segments << theme.style(style, "❯")
      "#{segments.join(' ')} "
    end

    def shorten_cwd(cwd, home)
      cwd = cwd.to_s
      home = home.to_s
      return cwd if home.empty?
      return "~" if cwd == home

      prefix = home.end_with?("/") ? home : "#{home}/"
      return "~#{cwd[home.length..]}" if cwd.start_with?(prefix)

      cwd
    end

    def format_duration(duration_ms)
      return nil unless duration_ms.to_i >= DURATION_THRESHOLD_MS

      format("%.1fs", duration_ms.to_f / 1000)
    end

    def warn_once(error)
      return if @warned

      @warned = true
      $stderr.puts("sharky: prompt error: #{error.message}")
    end
  end
end
