module Sharky
  module Jobs
    Job = Struct.new(:id, :pgid, :pids, :command, :state, :status, :notified, keyword_init: true) do
      def running?
        state == :running
      end

      def stopped?
        state == :stopped
      end

      def done?
        state == :done
      end
    end

    class Table
      def initialize
        @jobs = []
        @next_id = 1
        @statuses = {}
      end

      def add(command:, pgid:, pids:, state: :running, status: nil)
        job = Job.new(
          id: @next_id,
          pgid: pgid,
          pids: Array(pids),
          command: command,
          state: state,
          status: status,
          notified: false
        )
        @next_id += 1
        @jobs << job
        job
      end

      def all
        @jobs
      end

      def find(target)
        return target if target.is_a?(Job)

        text = target.to_s
        if text.start_with?("%")
          reference = text[1..]
          return current if reference.nil? || reference.empty? || reference == "+"
          return previous if reference == "-"
          return nil unless reference.match?(/\A\d+\z/)

          return @jobs.find { |job| job.id == reference.to_i }
        end
        return nil unless text.match?(/\A\d+\z/)

        @jobs.find { |job| job.id == text.to_i }
      end

      def current
        @jobs.reject(&:done?).max_by(&:id)
      end

      def previous
        @jobs.reject(&:done?).sort_by(&:id)[-2]
      end

      def remove(job)
        job = find(job) unless job.is_a?(Job)
        return unless job

        job.pids.each { |pid| @statuses.delete(pid) }
        @jobs.delete(job)
      end

      def reap
        loop do
          pid, status = Process.waitpid2(-1, Process::WNOHANG | Process::WUNTRACED)
          break unless pid

          record(pid, status)
        end
      rescue Errno::ECHILD
        nil
      end

      def wait(job)
        job.pids.each do |pid|
          next if @statuses.key?(pid)

          begin
            _, status = Process.waitpid2(pid, Process::WUNTRACED)
            record(pid, status)
          rescue Errno::ECHILD
            @statuses[pid] = nil
          end
        end
        update(job)
        status_for(job)
      end

      def resume(job)
        Process.kill("CONT", -job.pgid)
        job.pids.each { |pid| @statuses.delete(pid) if @statuses[pid]&.stopped? }
        job.state = :running
        job.status = nil
        job.notified = false
        true
      rescue SystemCallError
        false
      end

      private

      def record(pid, status)
        @statuses[pid] = status
        job = @jobs.find { |candidate| candidate.pids.include?(pid) }
        update(job) if job
      end

      def update(job)
        statuses = job.pids.map { |pid| @statuses[pid] }
        if statuses.any? { |status| status&.stopped? }
          job.state = :stopped
          job.status = nil
        elsif job.pids.all? { |pid| @statuses.key?(pid) }
          job.state = :done
          job.status = decode(statuses.last)
        end
      end

      def status_for(job)
        return job.status if job.status

        stopped = job.pids.map { |pid| @statuses[pid] }.find { |status| status&.stopped? }
        return 128 + stopped.stopsig if stopped

        1
      end

      def decode(status)
        return 1 if status.nil?
        return 128 + status.termsig if status.signaled?

        status.exitstatus || 1
      end
    end
  end
end
