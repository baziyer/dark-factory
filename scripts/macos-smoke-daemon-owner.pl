#!/usr/bin/perl
use strict;
use warnings;
use POSIX qw(WNOHANG);
if (($ARGV[0] // '') eq '--observe-fifo') {
    shift @ARGV;
    my ($fifo, $done, $seconds, $linger) = @ARGV;
    $linger //= 0;
    defined $seconds && $seconds =~ /^[1-9][0-9]*$/ && $linger =~ /^(?:0|[1-9][0-9]*)$/ or die "invalid FIFO observer\n";
    $SIG{ALRM} = sub { exit 124 };
    alarm $seconds;
    open my $reader, '<', $fifo or die "open FIFO: $!\n";
    my $read;
    1 while $read = sysread $reader, my $buffer, 4096;
    defined $read or die "read FIFO: $!\n";
    open my $marker, '>', $done or die "create FIFO marker: $!\n";
    close $marker or die "close FIFO marker: $!\n";
    select undef, undef, undef, $linger if $linger;
    exit 0;
}
my ($state, $expected_parent, $reap_marker, $program, @arguments) = @ARGV;
defined $program
    or die "usage: macos-smoke-daemon-owner.pl STATE PARENT REAP_MARKER PROGRAM [ARG ...]\n";
$expected_parent =~ /^[1-9][0-9]*$/ or die "expected parent PID is invalid\n";
getppid() == $expected_parent or die "daemon owner parent changed before startup\n";
sub mark {
    my ($name, $value) = @_;
    open my $file, '>', "$state/$name" or die "create $name: $!\n";
    print {$file} "$value\n" or die "write $name: $!\n";
    close $file or die "close $name: $!\n";
}
my $child = fork();
defined $child or die "fork daemon: $!\n";
if ($child == 0) {
    exec {$program} $program, @arguments;
    die "exec daemon: $!\n";
}

my $crashed = 0;
mark('owned-pid', $child);

sub finish_owned_child {
    my ($marker) = @_;
    kill 'TERM', $child;
    for (1 .. 100) {
        my $reaped = waitpid($child, WNOHANG);
        if ($reaped == $child) {
            mark($marker, $child);
            exit 0;
        }
        $reaped == 0 or die "poll daemon child for $marker: $!\n";
        select undef, undef, undef, 0.02;
    }
    kill 'KILL', $child;
    waitpid($child, 0) == $child or die "reap daemon child: $!\n";
    mark($marker, $child);
    exit 0;
}

$SIG{TERM} = sub {
    open my $marker, '>', $reap_marker;
    close $marker;
    finish_owned_child('reaped');
};

while (1) {
    if (getppid() != $expected_parent) {
        open my $marker, '>', $reap_marker;
        close $marker;
        finish_owned_child('reaped');
    }
    finish_owned_child('reaped') if -e "$state/reap";
    if (-e "$state/crash" && !$crashed) {
        my $signalled = kill 'KILL', $child;
        $signalled == 1 or die "kill owned daemon child: $!\n";
        $crashed = 1;
        mark('crashed', $child);
    }
    if (-e "$state/stop") {
        finish_owned_child('stopped');
    }
    select undef, undef, undef, 0.02;
}
