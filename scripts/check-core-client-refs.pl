#!/usr/bin/env perl
# Counts, per Rust file, the references to internal workspace crates that the
# core-client desktop must not make (ADR 0045); used by check-core-client-refs.sh.
#
# The source is tokenized (comments, string and character literals are removed),
# every `use` declaration is read up to its `;` and expanded as a tree, and the
# root crate of each imported path is resolved, so the form of the import does
# not matter:
#   use storage::Db;   use ::storage::Db;   use runtime as rt;   use storage;
#   use {runtime as rt, serde};   use a::{b, storage::{x}};   extern crate sandbox;
#   use {
#       runtime as rt,
#   };
# A name that a `use` or `mod` binds to something that is not an internal crate
# (`use super::config;`, `use tokio::runtime as rt;`) is a local name, and a path
# that starts with it is not a reference. A name that a `use` binds to an
# internal crate root (`use runtime as rt;`) is an alias, and `rt::X` counts too.
#
# One reference is counted for each root crate named by a `use` declaration, and
# one for each path in the code that starts at an internal crate or an alias.
#
# Usage: check-core-client-refs.pl <internal-crate-regex> <file.rs>...
# Output, one line per reference: `<file>\t<line>\t<text>`.
use strict;
use warnings;

my $internal_re = shift @ARGV or die "internal crate regex expected\n";
$internal_re =~ s/\A\(//;
$internal_re =~ s/\)\z//;
my %internal = map { $_ => 1 } split /\|/, $internal_re;

sub newlines { my ($s) = @_; return join '', ($s =~ /\n/g) }

# The source without comments, strings and character literals (newlines stay).
sub strip {
    my ($src) = @_;
    my $out = '';
    my $n = length $src;
    my $i = 0;
    while ($i < $n) {
        my $c = substr($src, $i, 1);
        my $two = substr($src, $i, 2);
        my ($head, $hashes);
        if (($c eq 'r' || $c eq 'b') && ($i == 0 || substr($src, $i - 1, 1) !~ /[A-Za-z0-9_]/)) {
            pos($src) = $i;
            ($head, $hashes) = ($1, $2) if $src =~ /\G(b?r(#*)")/gc;
        }
        if ($two eq '//') {
            my $e = index($src, "\n", $i);
            $i = $e < 0 ? $n : $e;
        } elsif ($two eq '/*') {
            my $depth = 1;
            $i += 2;
            while ($i < $n && $depth > 0) {
                my $t = substr($src, $i, 2);
                if ($t eq '/*') { $depth++; $i += 2 }
                elsif ($t eq '*/') { $depth--; $i += 2 }
                else { $out .= "\n" if substr($src, $i, 1) eq "\n"; $i++ }
            }
            $out .= ' ';
        } elsif (defined $head) {
            my $close = '"' . $hashes;
            my $e = index($src, $close, $i + length $head);
            $e = $n - length $close if $e < 0;
            $out .= newlines(substr($src, $i, $e + length($close) - $i)) . ' ';
            $i = $e + length $close;
        } elsif ($c eq '"') {
            $i++;
            while ($i < $n) {
                my $d = substr($src, $i, 1);
                if ($d eq '\\') { $out .= "\n" if substr($src, $i + 1, 1) eq "\n"; $i += 2; next }
                if ($d eq '"') { $i++; last }
                $out .= "\n" if $d eq "\n";
                $i++;
            }
            $out .= ' ';
        } elsif ($c eq "'") {
            # A character literal ('x', '\n', '\u{..}') or a lifetime ('a).
            if (substr($src, $i + 1, 1) eq '\\') {
                my $e = index($src, "'", $i + 3);
                $i = $e < 0 ? $n : $e + 1;
            } elsif (substr($src, $i + 2, 1) eq "'") {
                $i += 3;
            } else {
                $i++;
            }
            $out .= ' ';
        } else {
            $out .= $c;
            $i++;
        }
    }
    return $out;
}

sub tokenize {
    my ($text) = @_;
    my @tokens;
    my $line = 1;
    pos($text) = 0;
    while (pos($text) < length $text) {
        if ($text =~ /\G\n/gc) { $line++; next }
        if ($text =~ /\G\s+/gc) { next }
        if ($text =~ /\G(::)/gc) { push @tokens, [$1, $line]; next }
        if ($text =~ /\G(r#[A-Za-z_][A-Za-z0-9_]*|[A-Za-z_][A-Za-z0-9_]*)/gc) { push @tokens, [$1, $line]; next }
        if ($text =~ /\G([0-9][A-Za-z0-9_.]*)/gc) { push @tokens, ['0', $line]; next }
        if ($text =~ /\G(.)/gcs) { push @tokens, [$1, $line]; next }
    }
    return @tokens;
}

sub is_ident { return defined $_[0] && $_[0] =~ /\A[A-Za-z_]/ }

# The token text at index $i, or undef past the end (never grows the array).
sub tk {
    my ($tokens, $i) = @_;
    return $i >= 0 && $i < @$tokens ? $tokens->[$i][0] : undef;
}

# Parses a use tree at $$i; returns the leaves [path segments, alias], each path
# complete from the declaration's root (the braces are expanded).
sub parse_tree {
    my ($tokens, $i, @prefix) = @_;
    my @leaves;
    my @path = @prefix;
    $$i++ if (tk($tokens, $$i) // '') eq '::' && !@prefix;
    while (1) {
        my $t = tk($tokens, $$i);
        last unless defined $t;
        if ($t eq '{') {
            $$i++;
            while (defined tk($tokens, $$i) && tk($tokens, $$i) ne '}') {
                my $before = $$i;
                push @leaves, parse_tree($tokens, $i, @path);
                $$i++ if (tk($tokens, $$i) // '') eq ',';
                $$i++ if $$i == $before;    # never stall on an unexpected token
            }
            $$i++;    # }
            return @leaves;
        }
        if ($t eq '*') { $$i++; push @leaves, [[@path, '*'], undef]; return @leaves }
        last unless is_ident($t);
        push @path, $t;
        $$i++;
        if ((tk($tokens, $$i) // '') eq '::') { $$i++; next }
        my $alias;
        if ((tk($tokens, $$i) // '') eq 'as') {
            $$i++;
            $alias = tk($tokens, $$i);
            $$i++;
        }
        push @leaves, [[@path], $alias];
        return @leaves;
    }
    push @leaves, [[@path], undef] if @path > @prefix;
    return @leaves;
}

# The path of a leaf without a trailing `self` (`use a::{self}` imports `a`).
sub leaf_path {
    my ($leaf) = @_;
    my @p = @{ $leaf->[0] };
    pop @p if @p > 1 && $p[-1] eq 'self';
    return @p;
}

for my $file (@ARGV) {
    open my $fh, '<', $file or die "cannot read $file: $!\n";
    my $source = do { local $/; <$fh> };
    close $fh;
    my @lines = split /\n/, $source, -1;
    my @tokens = tokenize(strip($source));

    my (%local, %alias, @found, %in_use);
    # Pass 1: use declarations, `extern crate` and `mod` names.
    for (my $i = 0; $i < @tokens; $i++) {
        my $t = $tokens[$i][0];
        if ($t eq 'mod' && is_ident(tk(\@tokens, $i + 1))) {
            $local{ tk(\@tokens, $i + 1) } = 1;
        } elsif ($t eq 'extern' && (tk(\@tokens, $i + 1) // '') eq 'crate') {
            my $name = tk(\@tokens, $i + 2) // '';
            my $as = (tk(\@tokens, $i + 3) // '') eq 'as' ? tk(\@tokens, $i + 4) : undef;
            if ($internal{$name}) {
                push @found, [$tokens[$i][1], "extern crate $name"];
                $alias{$as} = 1 if defined $as;
            }
        } elsif ($t eq 'use') {
            my $j = $i + 1;
            my @leaves = parse_tree(\@tokens, \$j);
            $in_use{$_} = 1 for $i .. $j;
            my %roots;
            for my $leaf (@leaves) {
                my @p = leaf_path($leaf);
                next unless @p;
                if ($internal{ $p[0] }) {
                    $roots{ $p[0] } = 1;
                    $alias{ $leaf->[1] // $p[0] } = 1 if @p == 1;
                } else {
                    my $name = $leaf->[1] // $p[-1];
                    $local{$name} = 1 if $name ne '*';
                }
            }
            push @found, [$tokens[$i][1], "use of $_"] for sort keys %roots;
            $i = $j - 1;
        }
    }
    # Pass 2: paths in the code that start at an internal crate or an alias.
    for (my $i = 0; $i < @tokens; $i++) {
        next if $in_use{$i};
        my $t = $tokens[$i][0];
        next unless is_ident($t) && (tk(\@tokens, $i + 1) // '') eq '::';
        my $prev = tk(\@tokens, $i - 1) // '';
        next if $prev eq '.';    # a method turbofish, `x.config::<T>()`
        if ($prev eq '::') {
            # `a::b` continues a path; `x = ::b::c` and `(::b::c)` start one.
            my $before = tk(\@tokens, $i - 2) // '';
            next if is_ident($before) || $before eq '>' || $before eq ')' || $before eq ']';
        }
        if (($internal{$t} && !$local{$t}) || $alias{$t}) {
            push @found, [$tokens[$i][1], "path from $t"];
        }
    }
    for my $hit (sort { $a->[0] <=> $b->[0] } @found) {
        my $text = $lines[ $hit->[0] - 1 ] // '';
        $text =~ s/^\s+//;
        print "$file\t$hit->[0]\t$text\n";
    }
}
