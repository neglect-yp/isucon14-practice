package main

import (
	"golang.org/x/crypto/ssh"
	"io"
	"log"
	"net"
	"os"
	"time"
)

func main() {
	if len(os.Args) != 6 {
		log.Fatal("arguments: SSH_ADDRESS USER PRIVATE_KEY HOST_PUBLIC_KEY DOCKER_SOCKET")
	}
	key, err := os.ReadFile(os.Args[3])
	if err != nil {
		log.Fatal(err)
	}
	signer, err := ssh.ParsePrivateKey(key)
	if err != nil {
		log.Fatal(err)
	}
	hostBytes, err := os.ReadFile(os.Args[4])
	if err != nil {
		log.Fatal(err)
	}
	hostKey, _, _, _, err := ssh.ParseAuthorizedKey(hostBytes)
	if err != nil {
		log.Fatal(err)
	}
	config := &ssh.ClientConfig{User: os.Args[2], Auth: []ssh.AuthMethod{ssh.PublicKeys(signer)}, HostKeyCallback: ssh.FixedHostKey(hostKey), HostKeyAlgorithms: []string{ssh.KeyAlgoED25519}, Timeout: 10 * time.Second}
	client, err := ssh.Dial("tcp", os.Args[1], config)
	if err != nil {
		log.Fatal(err)
	}
	defer client.Close()
	session, err := client.NewSession()
	if err != nil {
		log.Fatal(err)
	}
	result, err := session.CombinedOutput(`sudo prlimit --pid "$PPID" --nofile=16384:1048576; cat /proc/$PPID/limits | sed -n '/open files/p'`)
	session.Close()
	if err != nil {
		log.Fatalf("session limit: %v %s", err, result)
	}
	log.Printf("forwarding session: %s", result)
	routes := [][4]string{{"unix", os.Args[5], "unix", "/var/run/docker.sock"}}
	for _, port := range []string{"8080", "3306", "12345"} {
		routes = append(routes,
			[4]string{"tcp4", "127.0.0.1:" + port, "tcp", "127.0.0.1:" + port},
			[4]string{"tcp6", "[::1]:" + port, "tcp", "127.0.0.1:" + port})
	}
	for _, route := range routes {
		listener, err := net.Listen(route[0], route[1])
		if err != nil {
			log.Fatal(err)
		}
		log.Printf("listening %s", route[1])
		go func(r [4]string, l net.Listener) {
			for {
				local, err := l.Accept()
				if err != nil {
					log.Fatal(err)
				}
				go func() {
					defer local.Close()
					remote, err := client.Dial(r[2], r[3])
					if err != nil {
						log.Printf("forward: %v", err)
						return
					}
					defer remote.Close()
					done := make(chan struct{}, 2)
					copyHalf := func(dst, src net.Conn) {
						_, err := io.Copy(dst, src)
						if err != nil {
							dst.Close()
							src.Close()
						} else if writer, ok := dst.(interface{ CloseWrite() error }); ok {
							writer.CloseWrite()
						}
						done <- struct{}{}
					}
					go copyHalf(remote, local)
					go copyHalf(local, remote)
					<-done
					<-done
				}()
			}
		}(route, listener)
	}
	log.Fatal(client.Wait())
}
